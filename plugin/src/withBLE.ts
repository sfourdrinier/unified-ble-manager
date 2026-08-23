import { type ConfigPlugin, createRunOncePlugin, withInfoPlist } from 'expo/config-plugins'

import { validateUnifiedBleExpoPluginOptions, type UnifiedBleExpoPluginOptions } from './expoPluginSchema'
import { isUnifiedBlePluginDebugEnabled, unifiedBlePluginDebugLog } from './debugLog'
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
  ...restorationInfoPlistKeys,
  ...appRestorationInfoPlistKeys,
  'UnifiedBleProtocolShowPowerAlert',
  'UnifiedBleProtocolNativeLogging'
])

function uniqueStrings(values: readonly string[]): string[] {
  return [...new Set(values)]
}

export function reconcileExpoInfoPlist(
  infoPlist: Record<string, unknown>,
  options: UnifiedBleExpoPluginOptions,
  _applicationId?: string
): Record<string, unknown> {
  for (const key of retiredInfoPlistKeys) delete infoPlist[key]
  for (const key of nativeConfigurationKeys) delete infoPlist[key]

  const bluetoothAlways = options.permissions?.bluetoothAlways
  if (typeof bluetoothAlways === 'string') infoPlist.NSBluetoothAlwaysUsageDescription = bluetoothAlways
  else delete infoPlist.NSBluetoothAlwaysUsageDescription

  const existingModes = Array.isArray(infoPlist.UIBackgroundModes)
    ? infoPlist.UIBackgroundModes.filter((mode): mode is string => typeof mode === 'string')
    : []
  const backgroundModes = uniqueStrings(existingModes.filter(mode => mode !== 'bluetooth-central'))
  if (options.background?.ios?.mode === 'central') backgroundModes.push('bluetooth-central')
  if (backgroundModes.length > 0) infoPlist.UIBackgroundModes = backgroundModes
  else delete infoPlist.UIBackgroundModes

  const restoration = options.background?.ios?.restoration
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
  config = withInfoPlist(config, infoPlistConfig => {
    reconcileExpoInfoPlist(infoPlistConfig.modResults, options)
    return infoPlistConfig
  })
  config = withBLEAndroidManifest(config, {
    requiredHardware: options.requiredHardware ?? false,
    neverForLocation: options.permissions?.android?.neverForLocation ?? false,
    legacyLocation: options.permissions?.android?.legacyLocation ?? 'none',
    nativeLogging: options.diagnostics?.nativeLogging
  })
  config = withBLEAndroidForegroundService(config, options.background?.android ?? { mode: 'none' })
  return config
}

export default createRunOncePlugin(withBLE, pkg.name, pkg.version)
