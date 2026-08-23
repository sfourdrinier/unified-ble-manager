import { AndroidConfig, XML } from 'expo/config-plugins'
import { resolve } from 'path'

import {
  addLocationPermissionToManifest,
  addScanPermissionToManifest,
  addBLEHardwareFeatureToManifest,
  reconcileBluetoothPermissions,
  reconcileExpoAndroidManifest
} from '../withBLEAndroidManifest'

const { readAndroidManifestAsync } = AndroidConfig.Manifest

const sampleManifestPath = resolve(__dirname, 'fixtures/AndroidManifest.xml')

describe('addLocationPermissionToManifest', () => {
  it(`adds elements`, async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest = addLocationPermissionToManifest(androidManifest, false)
    expect(androidManifest.manifest['uses-permission-sdk-23']).toContainEqual({
      $: {
        'android:name': 'android.permission.ACCESS_COARSE_LOCATION'
      }
    })
    expect(androidManifest.manifest['uses-permission-sdk-23']).toContainEqual({
      $: {
        'android:name': 'android.permission.ACCESS_FINE_LOCATION'
      }
    })
    // Sanity
    expect(XML.format(androidManifest)).toMatch(
      /<uses-permission-sdk-23 android:name="android\.permission\.ACCESS_COARSE_LOCATION"\/>/
    )
    expect(XML.format(androidManifest)).toMatch(
      /<uses-permission-sdk-23 android:name="android\.permission\.ACCESS_FINE_LOCATION"\/>/
    )
  })
  it(`adds elements with SDK limit`, async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest = addLocationPermissionToManifest(androidManifest, true)
    expect(androidManifest.manifest['uses-permission-sdk-23']).toContainEqual({
      $: {
        'android:name': 'android.permission.ACCESS_COARSE_LOCATION',
        'android:maxSdkVersion': '30'
      }
    })
    expect(androidManifest.manifest['uses-permission-sdk-23']).toContainEqual({
      $: {
        'android:name': 'android.permission.ACCESS_FINE_LOCATION',
        'android:maxSdkVersion': '30'
      }
    })
    // Sanity
    expect(XML.format(androidManifest)).toMatch(
      /<uses-permission-sdk-23 android:name="android\.permission\.ACCESS_COARSE_LOCATION" android:maxSdkVersion="30"\/>/
    )
    expect(XML.format(androidManifest)).toMatch(
      /<uses-permission-sdk-23 android:name="android\.permission\.ACCESS_FINE_LOCATION" android:maxSdkVersion="30"\/>/
    )
  })
})

describe('addScanPermissionToManifest', () => {
  it(`adds element`, async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest = addScanPermissionToManifest(androidManifest, false)
    expect(androidManifest.manifest['uses-permission']).toContainEqual({
      $: {
        'android:name': 'android.permission.BLUETOOTH_SCAN',
        'tools:targetApi': '31'
      }
    })
    // Sanity
    expect(XML.format(androidManifest)).toMatch(
      /<uses-permission android:name="android\.permission\.BLUETOOTH_SCAN" tools:targetApi="31"\/>/
    )
  })
  it(`adds element with 'neverForLocation' attribute`, async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest = addScanPermissionToManifest(androidManifest, true)
    expect(androidManifest.manifest['uses-permission']).toContainEqual({
      $: {
        'android:name': 'android.permission.BLUETOOTH_SCAN',
        'android:usesPermissionFlags': 'neverForLocation',
        'tools:targetApi': '31'
      }
    })
    // Sanity
    expect(XML.format(androidManifest)).toMatch(
      /<uses-permission android:name="android\.permission\.BLUETOOTH_SCAN" android:usesPermissionFlags="neverForLocation" tools:targetApi="31"\/>/
    )
  })
})

describe('reconcileBluetoothPermissions', () => {
  it('adds the bounded legacy and Android 12 Bluetooth permissions idempotently', async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest = reconcileBluetoothPermissions(androidManifest)
    androidManifest = reconcileBluetoothPermissions(androidManifest)

    const permissions = androidManifest.manifest['uses-permission'] ?? []
    expect(permissions.filter(item => item.$['android:name'] === 'android.permission.BLUETOOTH')).toEqual([
      { $: { 'android:name': 'android.permission.BLUETOOTH', 'android:maxSdkVersion': '30' } }
    ])
    expect(permissions.filter(item => item.$['android:name'] === 'android.permission.BLUETOOTH_ADMIN')).toEqual([
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN', 'android:maxSdkVersion': '30' } }
    ])
    expect(permissions.filter(item => item.$['android:name'] === 'android.permission.BLUETOOTH_CONNECT')).toEqual([
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT', 'tools:targetApi': '31' } }
    ])
  })

  it('repairs pre-existing unbounded Bluetooth permissions without adding duplicates', async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest.manifest['uses-permission'] = [
      { $: { 'android:name': 'android.permission.BLUETOOTH' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT' } }
    ]

    androidManifest = reconcileBluetoothPermissions(androidManifest)

    expect(androidManifest.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.BLUETOOTH', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT', 'tools:targetApi': '31' } }
    ])
  })
})

describe('addBLEHardwareFeatureToManifest', () => {
  it(`adds element`, async () => {
    let androidManifest = await readAndroidManifestAsync(sampleManifestPath)
    androidManifest = addBLEHardwareFeatureToManifest(androidManifest)

    expect(androidManifest.manifest['uses-feature']).toStrictEqual([
      {
        $: {
          'android:name': 'android.hardware.bluetooth_le',
          'android:required': 'true'
        }
      }
    ])
    // Sanity
    expect(XML.format(androidManifest)).toMatch(
      /<uses-feature android:name="android\.hardware\.bluetooth_le" android:required="true"\/>/
    )
  })
})

describe('requiredHardware reconciliation', () => {
  it('upgrades every existing BLE feature while preserving host ownership when disabled', () => {
    const manifest = {
      manifest: {
        'uses-feature': [
          { $: { 'android:name': 'android.hardware.bluetooth_le', 'android:required': 'false' } },
          { $: { 'android:name': 'android.hardware.bluetooth_le' } }
        ],
        application: [{ $: { 'android:name': '.MainApplication' }, 'meta-data': [] }]
      }
    }

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: true,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(manifest.manifest['uses-feature']).toEqual([
      {
        $: {
          'android:name': 'android.hardware.bluetooth_le',
          'android:required': 'true'
        }
      },
      {
        $: {
          'android:name': 'android.hardware.bluetooth_le',
          'android:required': 'true'
        }
      },
      {
        $: {
          'android:name': 'android.software.companion_device_setup',
          'android:required': 'false'
        }
      }
    ])
    expect(manifest.manifest.application[0]['meta-data']).toEqual(
      expect.arrayContaining([
        {
          $: {
            'android:name': 'com.sfourdrinier.unifiedblemanager.companion-device-setup-feature-ownership',
            'android:value': 'feature=companion_device_setup'
          }
        },
        {
          $: {
            'android:name': 'com.sfourdrinier.unifiedblemanager.expo.configuration-marker',
            'android:value': 'unified-ble-expo-v1'
          }
        },
        {
          $: {
            'android:name': 'com.sfourdrinier.unifiedblemanager.expo.legacy-location-policy',
            'android:value': 'none'
          }
        },
        {
          $: {
            'android:name': 'com.sfourdrinier.unifiedblemanager.expo.never-for-location',
            'android:value': 'false'
          }
        },
        {
          $: {
            'android:name': 'com.sfourdrinier.unifiedblemanager.expo.required-hardware',
            'android:value': 'true'
          }
        }
      ])
    )

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(manifest.manifest['uses-feature']).toEqual([
      {
        $: {
          'android:name': 'android.hardware.bluetooth_le',
          'android:required': 'true'
        }
      },
      {
        $: {
          'android:name': 'android.hardware.bluetooth_le',
          'android:required': 'true'
        }
      },
      {
        $: {
          'android:name': 'android.software.companion_device_setup',
          'android:required': 'false'
        }
      }
    ])
  })
})

describe('legacy location policy reconciliation', () => {
  const locationPermissionNames = [
    'android.permission.ACCESS_COARSE_LOCATION',
    'android.permission.ACCESS_FINE_LOCATION'
  ]

  function manifestWithHostLocationDeclarations() {
    return {
      manifest: {
        'uses-permission-sdk-23': locationPermissionNames.map(name => ({
          $: { 'android:name': name, 'android:maxSdkVersion': '29' }
        })),
        application: [{ $: { 'android:name': '.MainApplication' }, 'meta-data': [] }]
      }
    }
  }

  function locationPermissions(manifest: ReturnType<typeof manifestWithHostLocationDeclarations>) {
    return (manifest.manifest['uses-permission-sdk-23'] ?? []).filter(permission =>
      locationPermissionNames.includes(permission.$['android:name'])
    )
  }

  it('removes host location declarations for none while retaining managed Bluetooth permissions', () => {
    const manifest = manifestWithHostLocationDeclarations()

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(locationPermissions(manifest)).toEqual([])
    expect(manifest.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.BLUETOOTH', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_SCAN', 'tools:targetApi': '31' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT', 'tools:targetApi': '31' } }
    ])
  })

  it.each([
    ['required', false, {}],
    ['auto', true, { 'android:maxSdkVersion': '30' }]
  ] as const)('projects %s location policy through the host manifest', (policy, neverForLocation, attributes) => {
    const manifest = manifestWithHostLocationDeclarations()

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: false,
      neverForLocation,
      legacyLocation: policy
    })

    expect(locationPermissions(manifest)).toEqual(
      locationPermissionNames.map(name => ({ $: { 'android:name': name, ...attributes } }))
    )
    expect(manifest.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.BLUETOOTH', 'android:maxSdkVersion': '30' } },
      { $: { 'android:name': 'android.permission.BLUETOOTH_ADMIN', 'android:maxSdkVersion': '30' } },
      {
        $: {
          'android:name': 'android.permission.BLUETOOTH_SCAN',
          ...(neverForLocation ? { 'android:usesPermissionFlags': 'neverForLocation' } : {}),
          'tools:targetApi': '31'
        }
      },
      { $: { 'android:name': 'android.permission.BLUETOOTH_CONNECT', 'tools:targetApi': '31' } }
    ])
  })
})

describe('companion setup feature reconciliation', () => {
  const companionFeature = 'android.software.companion_device_setup'
  const ownershipMetadata = 'com.sfourdrinier.unifiedblemanager.companion-device-setup-feature-ownership'
  const ownershipValue = 'feature=companion_device_setup'
  const runtimeMetadata = [
    {
      $: {
        'android:name': 'com.sfourdrinier.unifiedblemanager.expo.configuration-marker',
        'android:value': 'unified-ble-expo-v1'
      }
    },
    {
      $: {
        'android:name': 'com.sfourdrinier.unifiedblemanager.expo.legacy-location-policy',
        'android:value': 'none'
      }
    },
    {
      $: {
        'android:name': 'com.sfourdrinier.unifiedblemanager.expo.never-for-location',
        'android:value': 'false'
      }
    },
    {
      $: {
        'android:name': 'com.sfourdrinier.unifiedblemanager.expo.required-hardware',
        'android:value': 'false'
      }
    }
  ]

  it('preserves a host-authored companion setup feature including required=true', () => {
    const manifest = {
      manifest: {
        'uses-feature': [{ $: { 'android:name': companionFeature, 'android:required': 'true' } }],
        application: [{ $: { 'android:name': '.MainApplication' }, 'meta-data': [] }]
      }
    }

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(manifest.manifest['uses-feature']).toEqual([
      { $: { 'android:name': companionFeature, 'android:required': 'true' } }
    ])
    expect(manifest.manifest.application[0]['meta-data']).toEqual(runtimeMetadata)
  })

  it('removes only the plugin-owned entry when a host declaration is added later', () => {
    const manifest = {
      manifest: {
        application: [{ $: { 'android:name': '.MainApplication' }, 'meta-data': [] }]
      }
    }

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })
    expect(manifest.manifest.application[0]['meta-data']).toEqual([
      { $: { 'android:name': ownershipMetadata, 'android:value': ownershipValue } },
      ...runtimeMetadata
    ])
    manifest.manifest['uses-feature']?.unshift({
      $: { 'android:name': companionFeature, 'android:required': 'true' }
    })

    reconcileExpoAndroidManifest(manifest, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })

    expect(manifest.manifest['uses-feature']).toEqual([
      { $: { 'android:name': companionFeature, 'android:required': 'true' } }
    ])
    expect(manifest.manifest.application[0]['meta-data']).toEqual(runtimeMetadata)
  })
})

jest.mock('expo/config', () => ({
  getNameFromConfig: () => ({ appName: 'App', webName: 'App' }),
  getConfig: () => ({ exp: { name: 'App', slug: 'app', web: {}, ios: {}, android: {} } })
}))
