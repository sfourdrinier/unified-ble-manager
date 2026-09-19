import { type ConfigPlugin, withAndroidManifest } from 'expo/config-plugins'
import type { AndroidBackgroundOptions } from './expoPluginSchema'
import type { AndroidManifestWithExtraTools, ManifestServiceWithExtraTools } from './withBLEAndroidManifest'

export const COMPANION_PRESENCE_SERVICE_NAME = 'com.sfourdrinier.unifiedblemanager.presence.UbmCompanionPresenceService'
export const COMPANION_PRESENCE_SERVICE_ACTION = 'android.companion.CompanionDeviceService'
export const COMPANION_PRESENCE_BIND_PERMISSION = 'android.permission.BIND_COMPANION_DEVICE_SERVICE'

type PluginIntentFilter = NonNullable<ManifestServiceWithExtraTools['intent-filter']>[number]

const PLUGIN_SERVICE_ATTRIBUTES = {
  'android:name': COMPANION_PRESENCE_SERVICE_NAME,
  'android:exported': 'true',
  'android:permission': COMPANION_PRESENCE_BIND_PERMISSION
} as const

function application(androidManifest: AndroidManifestWithExtraTools) {
  const result = androidManifest.manifest.application?.[0]
  if (!result) throw new Error('AndroidManifest.xml is missing the required application element')
  return result
}

function servicesOf(androidManifest: AndroidManifestWithExtraTools): ManifestServiceWithExtraTools[] {
  const app = application(androidManifest)
  if (!Array.isArray(app.service)) app.service = []
  return app.service
}

function isPluginIntentFilter(filter: PluginIntentFilter): boolean {
  const actions = filter.action
  if (!Array.isArray(actions) || actions.length !== 1) return false
  return actions[0].$['android:name'] === COMPANION_PRESENCE_SERVICE_ACTION
}

function isPluginOwnedService(service: ManifestServiceWithExtraTools): boolean {
  const { 'android:name': name, 'android:exported': exported, 'android:permission': permission, ...rest } = service.$
  // Exact shape: any host-added attribute makes the declaration host-owned.
  if (Object.keys(rest).length > 0) return false
  if (
    name !== PLUGIN_SERVICE_ATTRIBUTES['android:name'] ||
    exported !== PLUGIN_SERVICE_ATTRIBUTES['android:exported'] ||
    permission !== PLUGIN_SERVICE_ATTRIBUTES['android:permission']
  ) {
    return false
  }
  const filters = service['intent-filter']
  if (!Array.isArray(filters) || filters.length !== 1) return false
  return isPluginIntentFilter(filters[0])
}

function pluginServiceEntry(): ManifestServiceWithExtraTools {
  return {
    $: { ...PLUGIN_SERVICE_ATTRIBUTES },
    'intent-filter': [{ action: [{ $: { 'android:name': COMPANION_PRESENCE_SERVICE_ACTION } }] }]
  }
}

function addCompanionPresenceService(androidManifest: AndroidManifestWithExtraTools): void {
  const services = servicesOf(androidManifest)
  const existing = services.find(service => service.$['android:name'] === COMPANION_PRESENCE_SERVICE_NAME)
  // A host-declared entry with different attributes stays untouched: the
  // host owns the declaration and the plugin must not clobber it.
  if (existing !== undefined) return
  services.push(pluginServiceEntry())
}

function removeCompanionPresenceService(androidManifest: AndroidManifestWithExtraTools): void {
  const app = application(androidManifest)
  if (!Array.isArray(app.service)) return
  // Only the exact plugin shape is removed: a host-modified declaration is
  // host-owned and survives the disable.
  app.service = app.service.filter(service => !isPluginOwnedService(service))
}

export function reconcileAndroidCompanionPresence(
  androidManifest: AndroidManifestWithExtraTools,
  options: AndroidBackgroundOptions
): AndroidManifestWithExtraTools {
  if (options.mode === 'none') removeCompanionPresenceService(androidManifest)
  else addCompanionPresenceService(androidManifest)
  return androidManifest
}

export const withBLEAndroidCompanionPresence: ConfigPlugin<AndroidBackgroundOptions> = (config, options) =>
  withAndroidManifest(config, config => {
    reconcileAndroidCompanionPresence(config.modResults, options)
    return config
  })

export default withBLEAndroidCompanionPresence
