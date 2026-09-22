import { reconcileExpoAndroidManifest } from '../withBLEAndroidManifest'
import type { AndroidManifestWithExtraTools } from '../withBLEAndroidManifest'

export const CONTINUATION_METADATA_NAME = 'com.sfourdrinier.unifiedblemanager.BACKGROUND_CONTINUATION'

function emptyManifest(): AndroidManifestWithExtraTools {
  return {
    manifest: { application: [{ $: { 'android:name': '.MainApplication' } }] }
  } as unknown as AndroidManifestWithExtraTools
}

function metadataValue(manifest: AndroidManifestWithExtraTools): string | undefined {
  const metadata = manifest.manifest.application?.[0]?.['meta-data']
  const items = Array.isArray(metadata) ? metadata : metadata ? [metadata] : []
  return items.find(item => item.$?.['android:name'] === CONTINUATION_METADATA_NAME)?.$?.['android:value']
}

describe('Expo plugin background.continuation manifest default (BGS4)', () => {
  it('writes the canonical declaration as manifest meta-data when configured', () => {
    const manifest = reconcileExpoAndroidManifest(emptyManifest(), {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none',
      continuation: {
        onAppearance: 'native',
        peerId: 'A0:9E:1A:E9:B9:3D',
        resubscribe: [
          {
            serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
            serviceOccurrence: 1,
            characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
            characteristicOccurrence: 1
          }
        ]
      }
    })
    const parsed = JSON.parse(metadataValue(manifest) ?? 'null')
    expect(parsed).toMatchObject({
      onAppearance: 'native',
      peerId: 'A0:9E:1A:E9:B9:3D',
      resubscribe: [
        {
          serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
          characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb'
        }
      ]
    })
  })

  it('removes the meta-data when continuation is not configured', () => {
    const seeded = reconcileExpoAndroidManifest(emptyManifest(), {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none',
      continuation: { onAppearance: 'native', resubscribe: [] }
    })
    expect(metadataValue(seeded)).toBeDefined()
    const cleared = reconcileExpoAndroidManifest(seeded, {
      requiredHardware: false,
      neverForLocation: false,
      legacyLocation: 'none'
    })
    expect(metadataValue(cleared)).toBeUndefined()
  })
})
