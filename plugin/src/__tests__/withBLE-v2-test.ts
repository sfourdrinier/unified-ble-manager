import { reconcileExpoInfoPlist, validateUnifiedBleExpoPluginOptions } from '../withBLE'
import {
  deriveIosNativeProtocolRestoration as canonicalDeriveIosNativeProtocolRestoration,
  validateUnifiedBleExpoPluginOptions as canonicalValidateUnifiedBleExpoPluginOptions
} from '../expoPluginSchema'
import { validateBlePluginOptions } from '../withBLE'
import { reconcileExpoAndroidManifest } from '../withBLEAndroidManifest'

const validOptions = {
  requiredHardware: true,
  permissions: {
    bluetoothAlways: 'Allow $(PRODUCT_NAME) to connect to Bluetooth devices',
    android: {
      neverForLocation: true,
      legacyLocation: 'none'
    }
  },
  background: {
    ios: {
      mode: 'central',
      restoration: {
        id: 'primary',
        generation: '1'
      },
      showPowerAlert: false
    },
    android: {
      mode: 'none'
    }
  },
  diagnostics: {
    nativeLogging: 'events'
  }
}

describe('Expo plugin v2 schema', () => {
  it('uses the canonical schema module for compatibility validation and derivation', () => {
    expect(validateBlePluginOptions).toBe(canonicalValidateUnifiedBleExpoPluginOptions)
    expect(canonicalDeriveIosNativeProtocolRestoration).toBeDefined()
  })

  it('accepts the application-oriented schema without adding defaults', () => {
    expect(validateUnifiedBleExpoPluginOptions(validOptions)).toEqual(validOptions)
  })

  it.each([
    ['an unknown root key', { unexpected: true }],
    ['an unknown nested key', { permissions: { bluetoothAlways: 'ok', unexpected: true } }],
    ['an empty Bluetooth usage description', { permissions: { bluetoothAlways: '  ' } }],
    ['an invalid legacy location policy', { permissions: { android: { legacyLocation: 'sometimes' } } }],
    ['a malformed restoration id', { background: { ios: { mode: 'central', restoration: { id: ':bad' } } } }],
    ['a restoration without central mode', { background: { ios: { restoration: { id: 'primary' } } } }],
    ['an iOS peripheral mode', { background: { ios: { mode: 'peripheral' } } }],
    [
      'an Android foreground service without notification',
      {
        background: { android: { mode: 'connected-device-foreground-service' } }
      }
    ],
    [
      'notification fields on Android mode none',
      {
        background: {
          android: {
            mode: 'none',
            notification: { channelId: 'ble', channelName: 'BLE', title: 'BLE' }
          }
        }
      }
    ]
  ])('rejects %s', (_label, options) => {
    expect(() => validateUnifiedBleExpoPluginOptions(options)).toThrow()
  })
})

describe('Expo restoration derivation', () => {
  it('matches the versioned length-prefixed SHA-256 golden vector', () => {
    expect(
      canonicalDeriveIosNativeProtocolRestoration({
        applicationId: 'com.example.app',
        restorationId: 'primary',
        generation: '1'
      })
    ).toEqual({
      identifier: 'com.example.app.ubm.bUdTu5dedZHMiWeGqKUMUh',
      namespace: 'ubm-ns:48lbkVUlrryoqQSovTZJTCnAE4j3UAEovjqB_1yhpzA',
      epoch: '1',
      clientId: 'ubm-client:-kl_l-J6PglWQJ13nY8XmA2O5Rs6Kbfdxhsfi6H9AaE',
      hostSessionScope: 'ubm-host:dgnvuLv8MJc6IUqscDhR8M4AqPaQLkbKtEpy5oPdjGw'
    })
  })

  it('defaults generation deterministically and rejects invalid tokens', () => {
    expect(
      canonicalDeriveIosNativeProtocolRestoration({
        applicationId: 'com.example.app',
        restorationId: 'primary'
      }).epoch
    ).toBe('1')
    expect(() =>
      canonicalDeriveIosNativeProtocolRestoration({
        applicationId: 'com.example.app',
        restorationId: 'has space'
      })
    ).toThrow()
    expect(() =>
      canonicalDeriveIosNativeProtocolRestoration({
        applicationId: 'com.example.app',
        restorationId: 'primary',
        generation: 'x'.repeat(65)
      })
    ).toThrow()
  })
})

describe('Expo iOS reconciliation', () => {
  it('preserves a host-authored Bluetooth usage description when the plugin option is omitted or disabled', () => {
    const hostInfoPlist: Record<string, unknown> = {
      NSBluetoothAlwaysUsageDescription: 'Host-authored Bluetooth explanation'
    }

    const omitted = reconcileExpoInfoPlist({ ...hostInfoPlist }, {})
    const disabled = reconcileExpoInfoPlist({ ...hostInfoPlist }, { permissions: { bluetoothAlways: false } })

    expect(omitted.NSBluetoothAlwaysUsageDescription).toBe(hostInfoPlist.NSBluetoothAlwaysUsageDescription)
    expect(disabled.NSBluetoothAlwaysUsageDescription).toBe(hostInfoPlist.NSBluetoothAlwaysUsageDescription)
  })

  it('preserves a host-changed Bluetooth usage description when disabling prior plugin ownership', () => {
    const infoPlist: Record<string, unknown> = {}

    reconcileExpoInfoPlist(infoPlist, {
      permissions: { bluetoothAlways: 'Plugin-authored Bluetooth explanation' }
    })
    infoPlist.NSBluetoothAlwaysUsageDescription = 'Host-changed Bluetooth explanation'

    const disabled = reconcileExpoInfoPlist(infoPlist, { permissions: { bluetoothAlways: false } })

    expect(disabled.NSBluetoothAlwaysUsageDescription).toBe('Host-changed Bluetooth explanation')
    expect(disabled.UnifiedBlePluginBluetoothAlwaysUsageDescriptionOwnership).toBeUndefined()
  })

  it('is idempotent, removes stale managed keys, and preserves unrelated host config', () => {
    const infoPlist: Record<string, unknown> = {
      unrelated: 'preserve',
      NSBluetoothAlwaysUsageDescription: 'stale',
      UIBackgroundModes: ['audio', 'bluetooth-central', 'bluetooth-central'],
      BlePlxRestoreIdentifier: 'legacy',
      UnifiedBleProtocolRestoreIdentifier: 'stale',
      UnifiedBleProtocolRestorationNamespace: 'stale',
      UnifiedBleProtocolRestorationEpoch: 'stale',
      UnifiedBleProtocolRestorationClientId: 'stale',
      UnifiedBleProtocolRestorationHostSessionScope: 'stale',
      UnifiedBleProtocolNativeLogging: 'off'
    }

    const configured = reconcileExpoInfoPlist(infoPlist, validOptions, 'com.example.app')
    const twice = reconcileExpoInfoPlist(configured, validOptions, 'com.example.app')

    expect(twice).toEqual(configured)
    expect(configured).toMatchObject({
      unrelated: 'preserve',
      NSBluetoothAlwaysUsageDescription: validOptions.permissions.bluetoothAlways,
      UIBackgroundModes: ['audio', 'bluetooth-central'],
      UnifiedBleProtocolNativeLogging: 'events'
    })
    expect(configured.UnifiedBlePluginBluetoothAlwaysUsageDescriptionOwnership).toBe(
      validOptions.permissions.bluetoothAlways
    )
    expect(configured.BlePlxRestoreIdentifier).toBeUndefined()
    expect(configured.UnifiedBleProtocolRestoreIdentifier).toBeUndefined()
    expect(configured.UnifiedBleProtocolRestorationId).toBe('primary')
    expect(configured.UnifiedBleProtocolRestorationGeneration).toBe('1')

    const removed = reconcileExpoInfoPlist(configured, { permissions: { bluetoothAlways: false } }, 'com.example.app')

    expect(removed).toEqual({
      unrelated: 'preserve',
      UIBackgroundModes: ['audio']
    })
    expect(removed.UnifiedBlePluginBluetoothAlwaysUsageDescriptionOwnership).toBeUndefined()
  })
})

describe('Expo Android reconciliation', () => {
  it('deduplicates managed declarations, removes stale managed values, and preserves host values', () => {
    const manifest = {
      manifest: {
        $: { 'xmlns:tools': 'http://schemas.android.com/tools' },
        'uses-permission': [
          { $: { 'android:name': 'android.permission.INTERNET' } },
          { $: { 'android:name': 'android.permission.BLUETOOTH' } },
          { $: { 'android:name': 'android.permission.BLUETOOTH' } },
          { $: { 'android:name': 'android.permission.BLUETOOTH_SCAN' } }
        ],
        'uses-permission-sdk-23': [
          { $: { 'android:name': 'android.permission.ACCESS_FINE_LOCATION' } },
          { $: { 'android:name': 'android.permission.ACCESS_FINE_LOCATION' } }
        ],
        'uses-feature': [
          { $: { 'android:name': 'android.hardware.camera', 'android:required': 'false' } },
          { $: { 'android:name': 'android.hardware.bluetooth_le', 'android:required': 'true' } }
        ],
        application: [{ $: { 'android:name': '.MainApplication' }, 'meta-data': [] }]
      }
    }

    const configured = reconcileExpoAndroidManifest(manifest, {
      requiredHardware: true,
      neverForLocation: true,
      legacyLocation: 'none'
    })
    const twice = reconcileExpoAndroidManifest(configured, {
      requiredHardware: true,
      neverForLocation: true,
      legacyLocation: 'none'
    })

    expect(twice).toEqual(configured)
    expect(configured.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.INTERNET' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN', 'android:maxSdkVersion': '30' } },
      {
        $: {
          'android:name': 'android.permission.BLUETOOTH_SCAN',
          'android:usesPermissionFlags': 'neverForLocation',
          'tools:targetApi': '31'
        }
      },
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT', 'tools:targetApi': '31' } }
    ])
    expect(configured.manifest['uses-permission-sdk-23']).toBeUndefined()
    expect(configured.manifest['uses-feature']).toEqual([
      { $: { 'android:name': 'android.hardware.camera', 'android:required': 'false' } },
      { $: { 'android:name': 'android.hardware.bluetooth_le', 'android:required': 'true' } },
      { $: { 'android:name': 'android.software.companion_device_setup', 'android:required': 'false' } }
    ])

    const removed = reconcileExpoAndroidManifest(configured, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(removed.manifest['uses-feature']).toEqual([
      { $: { 'android:name': 'android.hardware.camera', 'android:required': 'false' } },
      { $: { 'android:name': 'android.hardware.bluetooth_le', 'android:required': 'true' } },
      { $: { 'android:name': 'android.software.companion_device_setup', 'android:required': 'false' } }
    ])
    expect(removed.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.INTERNET' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_SCAN', 'tools:targetApi': '31' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT', 'tools:targetApi': '31' } }
    ])
  })

  it('removes the BLE hardware feature when this plugin inserted and owned it', () => {
    const manifest = {
      manifest: {
        application: [{ $: { 'android:name': '.MainApplication' }, 'meta-data': [] }]
      }
    }

    const configured = reconcileExpoAndroidManifest(manifest, {
      requiredHardware: true,
      neverForLocation: false,
      legacyLocation: 'none'
    })
    const removed = reconcileExpoAndroidManifest(configured, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(removed.manifest['uses-feature']).toEqual([
      { $: { 'android:name': 'android.software.companion_device_setup', 'android:required': 'false' } }
    ])
  })
})
