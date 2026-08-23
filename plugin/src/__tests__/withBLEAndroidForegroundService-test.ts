import {
  FOREGROUND_SERVICE_OWNERSHIP_METADATA_NAME,
  reconcileAndroidForegroundService
} from '../withBLEAndroidForegroundService'

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

  it('records permission ownership only for declarations it inserts', () => {
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
        'android:value':
          'service=1;permissions=android.permission.FOREGROUND_SERVICE|android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE'
      }
    })
  })
})
