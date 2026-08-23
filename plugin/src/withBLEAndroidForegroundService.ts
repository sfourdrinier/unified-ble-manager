import { AndroidConfig, type ConfigPlugin, withAndroidManifest } from 'expo/config-plugins'
import type { AndroidBackgroundOptions } from './expoPluginSchema'
import type { AndroidManifestWithExtraTools } from './withBLEAndroidManifest'

export const BLE_FOREGROUND_SERVICE_NAME = 'com.sfourdrinier.unifiedblemanager.BlePlxForegroundService'
export const FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME =
  'com.sfourdrinier.unifiedblemanager.foreground-service-ownership'
export const FOREGROUND_SERVICE_NOTIFICATION_METADATA = Object.freeze({
  channelId: 'com.sfourdrinier.unifiedblemanager.foreground-service.channel-id',
  channelName: 'com.sfourdrinier.unifiedblemanager.foreground-service.channel-name',
  title: 'com.sfourdrinier.unifiedblemanager.foreground-service.title',
  body: 'com.sfourdrinier.unifiedblemanager.foreground-service.body',
  icon: 'com.sfourdrinier.unifiedblemanager.foreground-service.icon',
  restart: 'com.sfourdrinier.unifiedblemanager.foreground-service.restart'
})

export const FOREGROUND_SERVICE_PERMISSIONS = Object.freeze([
  'android.permission.FOREGROUND_SERVICE',
  'android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE',
  'android.permission.POST_NOTIFICATIONS'
])

type ManifestApplication = NonNullable<AndroidConfig.Manifest.AndroidManifest['manifest']['application']>[number]
type ManagedMetadata = NonNullable<ManifestApplication['meta-data']>[number]

function application(androidManifest: AndroidManifestWithExtraTools) {
  const result = androidManifest.manifest.application?.[0]
  if (!result) throw new Error('AndroidManifest.xml is missing the required application element')
  return result
}

function metadataFor(androidManifest: AndroidManifestWithExtraTools): ManagedMetadata[] {
  const app = application(androidManifest)
  const metadata = app['meta-data']
  if (!Array.isArray(metadata)) {
    app['meta-data'] = []
    return app['meta-data']
  }
  return metadata
}

function setMetadata(androidManifest: AndroidManifestWithExtraTools, name: string, value: string): void {
  const metadata = metadataFor(androidManifest)
  const existing = metadata.find(item => item.$['android:name'] === name)
  if (existing) existing.$['android:value'] = value
  else metadata.push({ $: { 'android:name': name, 'android:value': value } })
}

function removeManagedMetadata(androidManifest: AndroidManifestWithExtraTools): void {
  const app = application(androidManifest)
  if (!Array.isArray(app['meta-data'])) return
  const names = new Set([
    FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME,
    ...Object.values(FOREGROUND_SERVICE_NOTIFICATION_METADATA)
  ])
  const remaining = app['meta-data'].filter(item => !names.has(item.$['android:name']))
  if (remaining.length === 0) delete app['meta-data']
  else app['meta-data'] = remaining
}

function ownedPermissionNames(androidManifest: AndroidManifestWithExtraTools): Set<string> {
  const metadata = metadataFor(androidManifest)
  const marker = metadata.find(item => item.$['android:name'] === FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME)
  const markerValue = marker?.$['android:value']
  const markerParts = markerValue?.split(';') ?? []
  if (!markerParts.includes('service=1')) return new Set()
  const permissions = markerParts.find(value => value.startsWith('permissions='))?.slice('permissions='.length)
  return new Set(permissions === undefined || permissions.length === 0 ? [] : permissions.split('|'))
}

function setForegroundServiceOwnershipMetadata(
  androidManifest: AndroidManifestWithExtraTools,
  permissionNames: ReadonlySet<string>
): void {
  const permissions = [...permissionNames].join('|')
  setMetadata(
    androidManifest,
    FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME,
    permissions.length === 0 ? 'service=1' : `service=1;permissions=${permissions}`
  )
}

function addForegroundService(
  androidManifest: AndroidManifestWithExtraTools,
  options: Extract<AndroidBackgroundOptions, { mode: 'connected-device-foreground-service' }>
): void {
  const manifest = androidManifest.manifest
  if (!Array.isArray(manifest['uses-permission'])) manifest['uses-permission'] = []
  const permissions = manifest['uses-permission']
  const owned = ownedPermissionNames(androidManifest)
  AndroidConfig.Manifest.ensureToolsAvailable(androidManifest)
  const requiredPermissions = [
    { $: { 'android:name': 'android.permission.FOREGROUND_SERVICE' } },
    {
      $: {
        'android:name': 'android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE',
        'tools:targetApi': '34'
      }
    },
    {
      $: {
        'android:name': 'android.permission.POST_NOTIFICATIONS',
        'tools:targetApi': '33'
      }
    }
  ]
  for (const permission of requiredPermissions) {
    const name = permission.$['android:name']
    if (permissions.some(existing => existing.$['android:name'] === name)) continue
    permissions.push(permission)
    owned.add(name)
  }
  manifest['uses-permission'] = permissions

  const app = application(androidManifest)
  const services = Array.isArray(app.service) ? app.service : []
  app.service = services.filter(service => service.$['android:name'] !== BLE_FOREGROUND_SERVICE_NAME)
  app.service.push({
    $: {
      'android:name': BLE_FOREGROUND_SERVICE_NAME,
      'android:enabled': 'true',
      'android:exported': 'false',
      'android:foregroundServiceType': 'connectedDevice',
      'tools:targetApi': '29'
    }
  })

  setForegroundServiceOwnershipMetadata(androidManifest, owned)
  setMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.channelId, options.notification.channelId)
  setMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.channelName, options.notification.channelName)
  setMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.title, options.notification.title)
  if (options.notification.body === undefined) {
    removeMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.body)
  } else {
    setMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.body, options.notification.body)
  }
  if (options.notification.icon === undefined) {
    removeMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.icon)
  } else {
    setMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.icon, options.notification.icon)
  }
  if (options.restart === undefined) {
    removeMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.restart)
  } else {
    setMetadata(androidManifest, FOREGROUND_SERVICE_NOTIFICATION_METADATA.restart, options.restart)
  }
}

function removeMetadata(androidManifest: AndroidManifestWithExtraTools, name: string): void {
  const app = application(androidManifest)
  if (!Array.isArray(app['meta-data'])) return
  const remaining = app['meta-data'].filter(item => item.$['android:name'] !== name)
  if (remaining.length === 0) delete app['meta-data']
  else app['meta-data'] = remaining
}

function removeForegroundService(androidManifest: AndroidManifestWithExtraTools): void {
  const manifest = androidManifest.manifest
  const permissions = manifest['uses-permission']
  if (Array.isArray(permissions)) {
    const owned = ownedPermissionNames(androidManifest)
    manifest['uses-permission'] = permissions.filter(permission => {
      const name = permission.$['android:name']
      if (!owned.has(name)) return true
      owned.delete(name)
      return false
    })
  }
  const app = application(androidManifest)
  if (Array.isArray(app.service)) {
    app.service = app.service.filter(service => service.$['android:name'] !== BLE_FOREGROUND_SERVICE_NAME)
  }
  removeManagedMetadata(androidManifest)
}

export function reconcileAndroidForegroundService(
  androidManifest: AndroidManifestWithExtraTools,
  options: AndroidBackgroundOptions
): AndroidManifestWithExtraTools {
  if (options.mode === 'none') removeForegroundService(androidManifest)
  else addForegroundService(androidManifest, options)
  return androidManifest
}

export const withBLEAndroidForegroundService: ConfigPlugin<AndroidBackgroundOptions> = (config, options) =>
  withAndroidManifest(config, config => {
    reconcileAndroidForegroundService(config.modResults, options)
    return config
  })

export default withBLEAndroidForegroundService
