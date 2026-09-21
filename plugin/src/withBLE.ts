import { type ConfigPlugin, createRunOncePlugin, withInfoPlist } from 'expo/config-plugins'

import { validateUnifiedBleExpoPluginOptions, type UnifiedBleExpoPluginOptions } from './expoPluginSchema'
import { isUnifiedBlePluginDebugEnabled, unifiedBlePluginDebugLog } from './debugLog'
import { withBLEAndroidCompanionPresence } from './withBLEAndroidCompanionPresence'
import { withBLEAndroidForegroundService } from './withBLEAndroidForegroundService'
import { withBLEAndroidManifest } from './withBLEAndroidManifest'

type PackageMetadata = { readonly name: string; readonly version: string }
const pkg: PackageMetadata = require('../../package.json')

export { validateUnifiedBleExpoPluginOptions } from './expoPluginSchema'
export type { UnifiedBleExpoPluginOptions } from './expoPluginSchema'
export { validateUnifiedBleExpoPluginOptions as validateBlePluginOptions } from './expoPluginSchema'

const restorationInfoPlistKeys = Object.freeze([
  'UnifiedBleProtocolRestoreIdentifier',
  'UnifiedBleProtocolRestorationNamespace',
  'UnifiedBleProtocolRestorationEpoch',
  'UnifiedBleProtocolRestorationClientId',
  'UnifiedBleProtocolRestorationHostSessionScope'
])
const appRestorationInfoPlistKeys = Object.freeze([
  'UnifiedBleProtocolRestorationId',
  'UnifiedBleProtocolRestorationGeneration'
])
const retiredInfoPlistKeys = Object.freeze([
  'BlePlxRestoreIdentifier',
  'BlePlxRestorationNamespace',
  'BlePlxRestorationEpoch',
  'BlePlxRestorationClientId',
  'BlePlxRestorationHostSessionScope',
  'BlePlxDebugLogging'
])
const nativeConfigurationKeys = Object.freeze([
  'UnifiedBlePluginConfigurationMarker',
  ...restorationInfoPlistKeys,
  ...appRestorationInfoPlistKeys,
  'UnifiedBleProtocolShowPowerAlert',
  'UnifiedBleProtocolNativeLogging'
])
const bluetoothAlwaysUsageDescriptionOwnershipKey = 'UnifiedBlePluginBluetoothAlwaysUsageDescriptionOwnership'
const nativeConfigurationMarkerKey = 'UnifiedBlePluginConfigurationMarker'
const nativeConfigurationMarkerValue = 'unified-ble-expo-v1'

function uniqueStrings(values: readonly string[]): string[] {
  return [...new Set(values)]
}

/**
 * Whether this prebuild targets Apple TV / Android TV. The TV switch is the
 * documented Expo mechanism (`EXPO_TV=1`, see the Expo "Build Expo apps for
 * TV" guide): `@react-native-tvos/config-tv` only rewrites the native
 * project for TV when it is set, so this plugin keys its own TV behavior off
 * the same variable and phone prebuilds are unchanged.
 */
export function isExpoTvosPrebuild(environment: NodeJS.ProcessEnv = process.env): boolean {
  return environment.EXPO_TV === '1'
}

export function reconcileExpoInfoPlist(
  infoPlist: Record<string, unknown>,
  options: UnifiedBleExpoPluginOptions,
  _applicationId?: string,
  platform: 'ios' | 'tvos' = 'ios'
): Record<string, unknown> {
  for (const key of retiredInfoPlistKeys) delete infoPlist[key]
  for (const key of nativeConfigurationKeys) delete infoPlist[key]
  infoPlist[nativeConfigurationMarkerKey] = nativeConfigurationMarkerValue

  const bluetoothAlways = options.permissions?.bluetoothAlways
  if (typeof bluetoothAlways === 'string') {
    infoPlist.NSBluetoothAlwaysUsageDescription = bluetoothAlways
    infoPlist[bluetoothAlwaysUsageDescriptionOwnershipKey] = bluetoothAlways
  } else if (bluetoothAlways === false) {
    if (infoPlist[bluetoothAlwaysUsageDescriptionOwnershipKey] === infoPlist.NSBluetoothAlwaysUsageDescription) {
      delete infoPlist.NSBluetoothAlwaysUsageDescription
    }
    delete infoPlist[bluetoothAlwaysUsageDescriptionOwnershipKey]
  }

  // tvOS has no background Bluetooth mode and no state restoration, so a TV
  // build never declares them: the runtime then reports capability.unsupported
  // with the native reason instead of claiming something the OS cannot honor.
  const tvos = platform === 'tvos'
  const existingModes = Array.isArray(infoPlist.UIBackgroundModes)
    ? infoPlist.UIBackgroundModes.filter((mode): mode is string => typeof mode === 'string')
    : []
  const backgroundModes = uniqueStrings(existingModes.filter(mode => mode !== 'bluetooth-central'))
  if (!tvos && options.background?.ios?.mode === 'central') backgroundModes.push('bluetooth-central')
  if (backgroundModes.length > 0) infoPlist.UIBackgroundModes = backgroundModes
  else delete infoPlist.UIBackgroundModes

  const restoration = tvos ? undefined : options.background?.ios?.restoration
  if (restoration !== undefined) {
    infoPlist.UnifiedBleProtocolRestorationId = restoration.id
    infoPlist.UnifiedBleProtocolRestorationGeneration = restoration.generation ?? '1'
  }

  const showPowerAlert = options.background?.ios?.showPowerAlert
  if (showPowerAlert !== undefined) infoPlist.UnifiedBleProtocolShowPowerAlert = showPowerAlert
  const nativeLogging = options.diagnostics?.nativeLogging
  if (nativeLogging !== undefined) infoPlist.UnifiedBleProtocolNativeLogging = nativeLogging
  return infoPlist
}

const withBLE: ConfigPlugin<UnifiedBleExpoPluginOptions | void> = (config, props) => {
  const options = validateUnifiedBleExpoPluginOptions(props)
  const debugEnabled = isUnifiedBlePluginDebugEnabled()
  unifiedBlePluginDebugLog(debugEnabled, 'Plugin normalized options:', JSON.stringify(options))
  const tvosBuild = isExpoTvosPrebuild()
  unifiedBlePluginDebugLog(debugEnabled, 'TV prebuild (EXPO_TV=1):', tvosBuild)
  config = withInfoPlist(config, infoPlistConfig => {
    reconcileExpoInfoPlist(infoPlistConfig.modResults, options, undefined, tvosBuild ? 'tvos' : 'ios')
    return infoPlistConfig
  })
  config = withBLEAndroidManifest(config, {
    requiredHardware: options.requiredHardware ?? false,
    neverForLocation: options.permissions?.android?.neverForLocation ?? false,
    legacyLocation: options.permissions?.android?.legacyLocation ?? 'none',
    nativeLogging: options.diagnostics?.nativeLogging,
    ...(options.background?.continuation === undefined ? {} : { continuation: options.background.continuation })
  })
  config = withBLEAndroidForegroundService(config, options.background?.android ?? { mode: 'none' })
  config = withBLEAndroidCompanionPresence(config, options.background?.android ?? { mode: 'none' })
  return config
}

export default createRunOncePlugin(withBLE, pkg.name, pkg.version)
