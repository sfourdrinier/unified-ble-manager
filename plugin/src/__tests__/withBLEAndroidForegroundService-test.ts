import {
  FOREGROUND_SERVICE_PERMISSION_OWNERSHIP_METADATA_NAME,
  FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME,
  reconcileAndroidForegroundService
} from '../withBLEAndroidForegroundService'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const foregroundServiceOptions = {
  mode: 'connected-device-foreground-service' as const,
  notification: {
    channelId: 'ble',
    channelName: 'BLE',
    title: 'BLE'
  }
}

function applyForegroundService(
  manifest: {
    manifest: {
      application: Array<{
        $: { 'android:name': string }
        'meta-data'?: Array<{ $: Record<string, string> }>
        service?: Array<{ $: Record<string, string> }>
      }>
      'uses-permission'?: Array<{ $: Record<string, string> }>
    }
  },
  mode: typeof foregroundServiceOptions | { readonly mode: 'none' }
) {
  return reconcileAndroidForegroundService(manifest, mode)
}

describe('withBLEAndroidForegroundService ownership', () => {
  it('preserves generic foreground-service permissions when disabling without an ownership marker', () => {
    const manifest = {
      manifest: {
        $: { 'xmlns:android': 'http://schemas.android.com/apk/res/android' },
        'uses-permission': [
          { $: { 'android:name': 'android.permission.FOREGROUND_SERVICE' } },
          { $: { 'android:name': 'android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE' } },
          { $: { 'android:name': 'android.permission.POST_NOTIFICATIONS' } }
        ],
        application: [{ $: { 'android:name': '.MainApplication' } }]
      }
    }

    const configured = applyForegroundService(manifest, { mode: 'none' })

    expect(configured.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.FOREGROUND_SERVICE' } },
      { $: { 'android:name': 'android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE' } },
      { $: { 'android:name': 'android.permission.POST_NOTIFICATIONS' } }
    ])
  })

  it('removes only permissions inserted by this plugin after a foreground service is disabled', () => {
    const manifest = {
      manifest: {
        $: { 'xmlns:android': 'http://schemas.android.com/apk/res/android' },
        'uses-permission': [{ $: { 'android:name': 'android.permission.POST_NOTIFICATIONS' } }],
        application: [{ $: { 'android:name': '.MainApplication' } }]
      }
    }

    const configured = applyForegroundService(manifest, foregroundServiceOptions)
    const removed = applyForegroundService(configured, { mode: 'none' })

    expect(removed.manifest['uses-permission']).toEqual([
      { $: { 'android:name': 'android.permission.POST_NOTIFICATIONS' } }
    ])
    expect(removed.manifest.application[0].service).toEqual([])
    expect(removed.manifest.application[0]['meta-data']).toBeUndefined()
  })

  it('preserves a host-changed permission declaration when disabling prior plugin ownership', () => {
    const manifest = {
      manifest: {
        $: { 'xmlns:android': 'http://schemas.android.com/apk/res/android' },
        application: [{ $: { 'android:name': '.MainApplication' } }]
      }
    }

    const configured = applyForegroundService(manifest, foregroundServiceOptions)
    const connectedDevicePermission = configured.manifest['uses-permission']?.find(
      permission => permission.$['android:name'] === 'android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE'
    )
    if (!connectedDevicePermission) throw new Error('Expected the plugin to add the connected-device permission')
    connectedDevicePermission.$['tools:targetApi'] = '35'

    const removed = applyForegroundService(configured, { mode: 'none' })

    expect(removed.manifest['uses-permission']).toContainEqual({
      $: {
        'android:name': 'android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE',
        'tools:targetApi': '35'
      }
    })
  })

  it('emits the exact native service marker and a separate permission ownership marker', () => {
    const manifest = {
      manifest: {
        $: { 'xmlns:android': 'http://schemas.android.com/apk/res/android' },
        'uses-permission': [{ $: { 'android:name': 'android.permission.POST_NOTIFICATIONS' } }],
        application: [{ $: { 'android:name': '.MainApplication' } }]
      }
    }

    const configured = applyForegroundService(manifest, foregroundServiceOptions)
    const marker = configured.manifest.application[0]['meta-data']?.find(
      item => item.$['android:name'] === FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME
    )

    expect(marker).toEqual({
      $: {
        'android:name': FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME,
        'android:value': 'service=1'
      }
    })

    expect(configured.manifest.application[0]['meta-data']).toContainEqual({
      $: {
        'android:name': FOREGROUND_SERVICE_PERMISSION_OWNERSHIP_METADATA_NAME,
        'android:value':
          'permissions=android.permission.FOREGROUND_SERVICE|android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE'
      }
    })
  })

  it('guards the plugin marker against the native exact-value parser contract', () => {
    const nativeSource = readFileSync(
      resolve(
        __dirname,
        '../../../android/src/main/java/com/sfourdrinier/unifiedblemanager/background/ForegroundServiceNotificationConfiguration.java'
      ),
      'utf8'
    )

    expect(nativeSource).toContain('if (!"service=1".equals(metadata.get(OWNERSHIP_METADATA)))')

    const manifest = {
      manifest: {
        $: { 'xmlns:android': 'http://schemas.android.com/apk/res/android' },
        application: [{ $: { 'android:name': '.MainApplication' } }]
      }
    }

    const configured = applyForegroundService(manifest, foregroundServiceOptions)
    const serviceMarker = configured.manifest.application[0]['meta-data']?.find(
      item => item.$['android:name'] === FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME
    )

    expect(serviceMarker?.$['android:value']).toBe('service=1')
  })
})
