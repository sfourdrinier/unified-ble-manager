import { type ConfigPlugin, withAndroidManifest, AndroidConfig } from 'expo/config-plugins'
import type { LegacyLocationPolicy, NativeLoggingLevel } from './expoPluginSchema'
import { setUnifiedBleNativeLoggingAndroidManifest } from './withBLEDebugLogging'

type InnerManifest = AndroidConfig.Manifest.AndroidManifest['manifest']

type ManifestPermission = InnerManifest['permission']

type ExtraTools = {
  // https://developer.android.com/studio/write/tool-attributes#toolstargetapi
  'tools:targetApi'?: string
}

export type ManifestUsesPermissionWithExtraTools = {
  $: AndroidConfig.Manifest.ManifestUsesPermission['$'] & ExtraTools
}

type ManifestApplication = NonNullable<InnerManifest['application']>[number]

type ManifestService = NonNullable<ManifestApplication['service']>[number]

export type ManifestServiceWithExtraTools = Omit<ManifestService, '$'> & {
  $: ManifestService['$'] & ExtraTools
}

type ManifestApplicationWithExtraTools = Omit<ManifestApplication, 'service'> & {
  service?: ManifestServiceWithExtraTools[]
}

const BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_NAME = 'com.sfourdrinier.unifiedblemanager.bluetooth-le-feature-ownership'
const BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_VALUE = 'feature=bluetooth_le'

export type AndroidManifestWithExtraTools = {
  manifest: Omit<InnerManifest, 'application' | 'uses-permission' | 'uses-permission-sdk-23'> & {
    application?: ManifestApplicationWithExtraTools[]
    permission?: ManifestPermission
    'uses-permission'?: ManifestUsesPermissionWithExtraTools[]
    'uses-permission-sdk-23'?: ManifestUsesPermissionWithExtraTools[]
    'uses-feature'?: InnerManifest['uses-feature']
  }
}

type ManagedBluetoothPermission = {
  readonly name: string
  readonly attributes: Readonly<Record<string, string>>
}

const managedBluetoothPermissions: readonly ManagedBluetoothPermission[] = Object.freeze([
  Object.freeze({
    name: 'android.permission.BLUETOOTH',
    attributes: Object.freeze({ 'android:maxSdkVersion': '30' })
  }),
  Object.freeze({
    name: 'android.permission.BLUETOOTH_ADMIN',
    attributes: Object.freeze({ 'android:maxSdkVersion': '30' })
  }),
  Object.freeze({
    name: 'android.permission.BLUETOOTH_CONNECT',
    attributes: Object.freeze({ 'tools:targetApi': '31' })
  })
])

export const withBLEAndroidManifest: ConfigPlugin<{
  requiredHardware: boolean
  neverForLocation: boolean
  legacyLocation: LegacyLocationPolicy
  nativeLogging?: NativeLoggingLevel
}> = (config, { requiredHardware, neverForLocation, legacyLocation, nativeLogging }) =>
  withAndroidManifest(config, config => {
    config.modResults = reconcileExpoAndroidManifest(config.modResults, {
      requiredHardware,
      neverForLocation,
      legacyLocation
    })
    setUnifiedBleNativeLoggingAndroidManifest(config.modResults, nativeLogging)
    return config
  })

export interface ExpoAndroidManifestOptions {
  readonly requiredHardware: boolean
  readonly neverForLocation: boolean
  readonly legacyLocation: LegacyLocationPolicy
  readonly nativeLogging?: NativeLoggingLevel
}

/** Applies the complete managed Android projection with stable ordering and removal. */
export function reconcileExpoAndroidManifest(
  androidManifest: AndroidManifestWithExtraTools,
  options: ExpoAndroidManifestOptions
): AndroidManifestWithExtraTools {
  reconcileBluetoothPermissions(androidManifest)
  reconcileScanPermission(androidManifest, options.neverForLocation)
  reconcileLegacyLocationPermissions(androidManifest, options.legacyLocation, options.neverForLocation)
  reconcileBLEHardwareFeature(androidManifest, options.requiredHardware)
  reconcileCompanionSetupFeature(androidManifest)
  return androidManifest
}

/**
 * Keeps one canonical declaration for each permission whose Android platform
 * semantics changed at API 31.  Expo's generic permission helper can append
 * an unbounded legacy declaration, so this reconciles both new and existing
 * manifests instead of relying on append-only behavior.
 */
export function reconcileBluetoothPermissions(
  androidManifest: AndroidManifestWithExtraTools
): AndroidManifestWithExtraTools {
  if (!Array.isArray(androidManifest.manifest['uses-permission'])) {
    androidManifest.manifest['uses-permission'] = []
  }

  ensureToolsAvailable(androidManifest)
  const existingPermissions = androidManifest.manifest['uses-permission']
  const unmatchedPermissions: ManifestUsesPermissionWithExtraTools[] = []
  const reconciledPermissions = new Map<string, ManifestUsesPermissionWithExtraTools>()

  for (const permission of existingPermissions) {
    const required = managedBluetoothPermissions.find(candidate => candidate.name === permission.$['android:name'])
    if (required === undefined) {
      unmatchedPermissions.push(permission)
      continue
    }

    const prior = reconciledPermissions.get(required.name)
    if (prior === undefined) {
      reconciledPermissions.set(required.name, {
        ...permission,
        $: {
          ...permission.$,
          ...required.attributes
        }
      })
      continue
    }

    prior.$ = {
      ...prior.$,
      ...permission.$,
      ...required.attributes
    }
  }

  for (const required of managedBluetoothPermissions) {
    if (reconciledPermissions.has(required.name)) continue
    reconciledPermissions.set(required.name, {
      $: {
        'android:name': required.name,
        ...required.attributes
      }
    })
  }

  androidManifest.manifest['uses-permission'] = [
    ...unmatchedPermissions,
    ...managedBluetoothPermissions.map(required => {
      const permission = reconciledPermissions.get(required.name)
      if (permission === undefined) {
        throw new Error(`BLE permission reconciliation failed for ${required.name}`)
      }
      return permission
    })
  ]
  return androidManifest
}

/**
 * Add location permissions
 *  - 'android.permission.ACCESS_COARSE_LOCATION' for Android SDK 28 (Android 9) and lower
 *  - 'android.permission.ACCESS_FINE_LOCATION' for Android SDK 29 (Android 10) and higher.
 *    From Android SDK 31 (Android 12) it might not be required if BLE is not used for location.
 */
export function addLocationPermissionToManifest(
  androidManifest: AndroidManifestWithExtraTools,
  neverForLocationSinceSdk31: boolean
): AndroidManifestWithExtraTools {
  if (!Array.isArray(androidManifest.manifest['uses-permission-sdk-23'])) {
    androidManifest.manifest['uses-permission-sdk-23'] = []
  }

  const optMaxSdkVersion = neverForLocationSinceSdk31
    ? {
        'android:maxSdkVersion': '30'
      }
    : {}

  if (
    !androidManifest.manifest['uses-permission-sdk-23'].find(
      item => item.$['android:name'] === 'android.permission.ACCESS_COARSE_LOCATION'
    )
  ) {
    androidManifest.manifest['uses-permission-sdk-23'].push({
      $: {
        'android:name': 'android.permission.ACCESS_COARSE_LOCATION',
        ...optMaxSdkVersion
      }
    })
  }

  if (
    !androidManifest.manifest['uses-permission-sdk-23'].find(
      item => item.$['android:name'] === 'android.permission.ACCESS_FINE_LOCATION'
    )
  ) {
    androidManifest.manifest['uses-permission-sdk-23'].push({
      $: {
        'android:name': 'android.permission.ACCESS_FINE_LOCATION',
        ...optMaxSdkVersion
      }
    })
  }

  return androidManifest
}

/**
 * Add 'android.permission.BLUETOOTH_SCAN'.
 * Required since Android SDK 31 (Android 12).
 */
export function addScanPermissionToManifest(
  androidManifest: AndroidManifestWithExtraTools,
  neverForLocation: boolean
): AndroidManifestWithExtraTools {
  if (!Array.isArray(androidManifest.manifest['uses-permission'])) {
    androidManifest.manifest['uses-permission'] = []
  }

  if (
    !androidManifest.manifest['uses-permission'].find(
      item => item.$['android:name'] === 'android.permission.BLUETOOTH_SCAN'
    )
  ) {
    AndroidConfig.Manifest.ensureToolsAvailable(androidManifest)
    androidManifest.manifest['uses-permission']?.push({
      $: {
        'android:name': 'android.permission.BLUETOOTH_SCAN',
        ...(neverForLocation
          ? {
              'android:usesPermissionFlags': 'neverForLocation'
            }
          : {}),
        'tools:targetApi': '31'
      }
    })
  }
  return androidManifest
}

// Add this line if your application always requires BLE. More info can be found on: https://developer.android.com/guide/topics/connectivity/bluetooth-le.html#permissions
export function addBLEHardwareFeatureToManifest(
  androidManifest: AndroidConfig.Manifest.AndroidManifest
): AndroidConfig.Manifest.AndroidManifest {
  // Add `<uses-feature android:name="android.hardware.bluetooth_le" android:required="true"/>` to the AndroidManifest.xml
  if (!Array.isArray(androidManifest.manifest['uses-feature'])) {
    androidManifest.manifest['uses-feature'] = []
  }

  if (
    !androidManifest.manifest['uses-feature'].find(item => item.$['android:name'] === 'android.hardware.bluetooth_le')
  ) {
    androidManifest.manifest['uses-feature']?.push({
      $: {
        'android:name': 'android.hardware.bluetooth_le',
        'android:required': 'true'
      }
    })
  }
  return androidManifest
}

function reconcileScanPermission(androidManifest: AndroidManifestWithExtraTools, neverForLocation: boolean): void {
  if (!Array.isArray(androidManifest.manifest['uses-permission'])) {
    androidManifest.manifest['uses-permission'] = []
  }
  const scanPermissions = androidManifest.manifest['uses-permission'].filter(
    item => item.$['android:name'] === 'android.permission.BLUETOOTH_SCAN'
  )
  androidManifest.manifest['uses-permission'] = androidManifest.manifest['uses-permission'].filter(
    item => item.$['android:name'] !== 'android.permission.BLUETOOTH_SCAN'
  )
  ensureToolsAvailable(androidManifest)
  const existing = scanPermissions[0]
  const scanPermission = {
    $: {
      ...(existing?.$ ?? {}),
      'android:name': 'android.permission.BLUETOOTH_SCAN',
      ...(neverForLocation ? { 'android:usesPermissionFlags': 'neverForLocation' } : {}),
      'tools:targetApi': '31'
    }
  }
  if (!neverForLocation) delete scanPermission.$['android:usesPermissionFlags']

  const permissions = androidManifest.manifest['uses-permission']
  const connectIndex = permissions.findIndex(item => item.$['android:name'] === 'android.permission.BLUETOOTH_CONNECT')
  permissions.splice(connectIndex < 0 ? permissions.length : connectIndex, 0, scanPermission)
}

function ensureToolsAvailable(androidManifest: AndroidManifestWithExtraTools): void {
  if (androidManifest.manifest.$ === undefined) {
    androidManifest.manifest.$ = {
      'xmlns:android': 'http://schemas.android.com/apk/res/android'
    }
  }
  AndroidConfig.Manifest.ensureToolsAvailable(androidManifest)
}

function reconcileLegacyLocationPermissions(
  androidManifest: AndroidManifestWithExtraTools,
  legacyLocation: LegacyLocationPolicy,
  neverForLocation: boolean
): void {
  const existing = Array.isArray(androidManifest.manifest['uses-permission-sdk-23'])
    ? androidManifest.manifest['uses-permission-sdk-23']
    : []
  const retained = existing.filter(
    item =>
      item.$['android:name'] !== 'android.permission.ACCESS_COARSE_LOCATION' &&
      item.$['android:name'] !== 'android.permission.ACCESS_FINE_LOCATION'
  )
  if (legacyLocation === 'none') {
    if (retained.length === 0) delete androidManifest.manifest['uses-permission-sdk-23']
    else androidManifest.manifest['uses-permission-sdk-23'] = retained
    return
  }

  const maxSdkVersion = legacyLocation === 'auto' && neverForLocation ? '30' : undefined
  const locationAttributes = (name: string) => ({
    $: {
      'android:name': name,
      ...(maxSdkVersion === undefined ? {} : { 'android:maxSdkVersion': maxSdkVersion })
    }
  })
  androidManifest.manifest['uses-permission-sdk-23'] = [
    ...retained,
    locationAttributes('android.permission.ACCESS_COARSE_LOCATION'),
    locationAttributes('android.permission.ACCESS_FINE_LOCATION')
  ]
}

function reconcileBLEHardwareFeature(androidManifest: AndroidManifestWithExtraTools, requiredHardware: boolean): void {
  const existingFeatures = Array.isArray(androidManifest.manifest['uses-feature'])
    ? androidManifest.manifest['uses-feature']
    : []
  const bluetoothFeatures = existingFeatures.filter(
    feature => feature.$['android:name'] === 'android.hardware.bluetooth_le'
  )
  const features = existingFeatures.filter(feature => feature.$['android:name'] !== 'android.hardware.bluetooth_le')
  const owned = hasMetadata(androidManifest, BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_NAME)

  if (requiredHardware && bluetoothFeatures.length === 0) {
    features.push({
      $: {
        'android:name': 'android.hardware.bluetooth_le',
        'android:required': 'true'
      }
    })
    setMetadata(
      androidManifest,
      BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_NAME,
      BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_VALUE
    )
  } else if (!requiredHardware && owned) {
    bluetoothFeatures.shift()
    removeMetadata(androidManifest, BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_NAME)
  } else {
    features.push(...bluetoothFeatures)
  }

  if (features.length === 0) delete androidManifest.manifest['uses-feature']
  else androidManifest.manifest['uses-feature'] = features
}

function hasMetadata(androidManifest: AndroidManifestWithExtraTools, name: string): boolean {
  const application = androidManifest.manifest.application?.[0]
  const metadata = application?.['meta-data']
  return (
    Array.isArray(metadata) &&
    metadata.some(
      item =>
        item.$?.['android:name'] === name && item.$?.['android:value'] === BLE_HARDWARE_FEATURE_OWNERSHIP_METADATA_VALUE
    )
  )
}

function setMetadata(androidManifest: AndroidManifestWithExtraTools, name: string, value: string): void {
  const application = androidManifest.manifest.application?.[0]
  if (!application) throw new Error('AndroidManifest.xml is missing the required application element')
  const currentMetadata = application['meta-data']
  const metadata = Array.isArray(currentMetadata) ? currentMetadata : currentMetadata ? [currentMetadata] : []
  application['meta-data'] = metadata
  const existing = metadata.find(item => item.$?.['android:name'] === name)
  if (existing) existing.$['android:value'] = value
  else metadata.push({ $: { 'android:name': name, 'android:value': value } })
}

function removeMetadata(androidManifest: AndroidManifestWithExtraTools, name: string): void {
  const application = androidManifest.manifest.application?.[0]
  if (!application) throw new Error('AndroidManifest.xml is missing the required application element')
  const metadata = application['meta-data']
  if (!Array.isArray(metadata)) return
  const remaining = metadata.filter(item => item.$?.['android:name'] !== name)
  if (remaining.length === 0) delete application['meta-data']
  else application['meta-data'] = remaining
}

function reconcileCompanionSetupFeature(androidManifest: AndroidManifestWithExtraTools): void {
  const features = Array.isArray(androidManifest.manifest['uses-feature'])
    ? androidManifest.manifest['uses-feature'].filter(
        feature => feature.$['android:name'] !== 'android.software.companion_device_setup'
      )
    : []
  features.push({
    $: {
      'android:name': 'android.software.companion_device_setup',
      'android:required': 'false'
    }
  })
  androidManifest.manifest['uses-feature'] = features
}
