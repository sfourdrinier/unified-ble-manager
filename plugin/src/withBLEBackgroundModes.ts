import { type ConfigPlugin, withInfoPlist } from 'expo/config-plugins'

export enum BackgroundMode {
  Central = 'central'
}

function ensureKey(arr: string[], key: string) {
  if (!arr.find(mode => mode === key)) {
    arr.push(key)
  }
  return arr
}

const centralKey = 'bluetooth-central'

/**
 * Append `UIBackgroundModes` to the `Info.plist`.
 */
export const withBLEBackgroundModes: ConfigPlugin<BackgroundMode[]> = (c, modes) =>
  withInfoPlist(c, config => {
    if (!Array.isArray(config.modResults.UIBackgroundModes)) {
      config.modResults.UIBackgroundModes = []
    }

    if (modes.includes(BackgroundMode.Central)) {
      config.modResults.UIBackgroundModes = ensureKey(config.modResults.UIBackgroundModes, centralKey)
    }

    // Prevent empty array
    if (!config.modResults.UIBackgroundModes.length) {
      delete config.modResults.UIBackgroundModes
    }

    return config
  })
