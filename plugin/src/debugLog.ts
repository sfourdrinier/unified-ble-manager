export function isUnifiedBlePluginDebugEnabled(propDebug?: boolean): boolean {
  if (propDebug === true) return true

  const env = process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG ?? process.env.BLEPLX_PLUGIN_DEBUG
  if (!env) return false

  return env === '1' || env.toLowerCase() === 'true' || env.toLowerCase() === 'yes'
}

export function unifiedBlePluginDebugLog(enabled: boolean, ...args: unknown[]): void {
  if (!enabled) return
  // eslint-disable-next-line no-console
  console.log('[UNIFIED_BLE_MANAGER_PLUGIN]', ...args)
}

/** @deprecated Use isUnifiedBlePluginDebugEnabled. */
export const isBlePlxPluginDebugEnabled = isUnifiedBlePluginDebugEnabled
/** @deprecated Use unifiedBlePluginDebugLog. */
export const blePlxPluginDebugLog = unifiedBlePluginDebugLog
