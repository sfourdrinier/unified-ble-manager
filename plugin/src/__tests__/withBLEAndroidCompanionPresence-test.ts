import {
  COMPANION_PRESENCE_BIND_PERMISSION,
  COMPANION_PRESENCE_SERVICE_ACTION,
  COMPANION_PRESENCE_SERVICE_NAME,
  reconcileAndroidCompanionPresence
} from '../withBLEAndroidCompanionPresence'

const connectedDeviceOptions = {
  mode: 'connected-device-foreground-service' as const,
  notification: {
    channelId: 'ble',
    channelName: 'BLE',
    title: 'BLE'
  }
}

type ManifestService = {
  $: Record<string, string>
  'intent-filter'?: Array<{ action: Array<{ $: Record<string, string> }> }>
}

function emptyManifest() {
  return {
    manifest: {
      $: { 'xmlns:android': 'http://schemas.android.com/apk/res/android' },
      application: [{ $: { 'android:name': '.MainApplication' } }]
    }
  }
}

function servicesOf(manifest: { manifest: { application: Array<{ service?: ManifestService[] }> } }) {
  return manifest.manifest.application[0].service ?? []
}

describe('withBLEAndroidCompanionPresence', () => {
  it('declares the permission-gated presence service when background android is configured', () => {
    const configured = reconcileAndroidCompanionPresence(emptyManifest(), connectedDeviceOptions)

    expect(servicesOf(configured)).toContainEqual({
      $: {
        'android:name': COMPANION_PRESENCE_SERVICE_NAME,
        'android:exported': 'true',
        'android:permission': COMPANION_PRESENCE_BIND_PERMISSION
      },
      'intent-filter': [{ action: [{ $: { 'android:name': COMPANION_PRESENCE_SERVICE_ACTION } }] }]
    })
  })

  it('is idempotent: applying twice leaves a single presence service entry', () => {
    const once = reconcileAndroidCompanionPresence(emptyManifest(), connectedDeviceOptions)
    const twice = reconcileAndroidCompanionPresence(once, connectedDeviceOptions)

    expect(
      servicesOf(twice).filter(service => service.$['android:name'] === COMPANION_PRESENCE_SERVICE_NAME)
    ).toHaveLength(1)
  })

  it('removes only the plugin presence service when background android is disabled', () => {
    const configured = reconcileAndroidCompanionPresence(emptyManifest(), connectedDeviceOptions)
    configured.manifest.application[0].service?.push({ $: { 'android:name': '.HostService' } })
    const removed = reconcileAndroidCompanionPresence(configured, { mode: 'none' })

    expect(servicesOf(removed)).toEqual([{ $: { 'android:name': '.HostService' } }])
  })

  it('preserves a host-modified presence service declaration when disabling', () => {
    const configured = reconcileAndroidCompanionPresence(emptyManifest(), connectedDeviceOptions)
    const presence = servicesOf(configured).find(
      service => service.$['android:name'] === COMPANION_PRESENCE_SERVICE_NAME
    )
    if (!presence) throw new Error('Expected the plugin to add the presence service')
    presence.$['android:description'] = '@string/host_description'

    const removed = reconcileAndroidCompanionPresence(configured, { mode: 'none' })

    expect(servicesOf(removed)).toContainEqual(presence)
  })

  it('leaves a host-declared presence service untouched when enabling', () => {
    const manifest = emptyManifest()
    const hostDeclared: ManifestService = {
      $: { 'android:name': COMPANION_PRESENCE_SERVICE_NAME, 'android:exported': 'false' }
    }
    manifest.manifest.application[0].service = [hostDeclared]

    const configured = reconcileAndroidCompanionPresence(manifest, connectedDeviceOptions)

    expect(servicesOf(configured)).toEqual([hostDeclared])
  })
})
