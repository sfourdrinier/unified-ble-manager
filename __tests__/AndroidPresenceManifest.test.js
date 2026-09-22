'use strict'

// Finding 226: presence observation (CompanionDeviceManager.startObservingDevicePresence)
// requires REQUEST_OBSERVE_COMPANION_DEVICE_PRESENCE. The library manifest never declared
// it, so every consuming app got permission.denied and Android cold-start restoration
// (#212) could not be armed at all — proven on a physical Samsung SM-A376U1.
const fs = require('node:fs')
const path = require('node:path')

const MANIFEST = path.join(__dirname, '..', 'android', 'src', 'main', 'AndroidManifest.xml')
const PRESENCE_PERMISSION = 'android.permission.REQUEST_OBSERVE_COMPANION_DEVICE_PRESENCE'

describe('the Android library manifest declares what its features need', () => {
  const manifest = fs.readFileSync(MANIFEST, 'utf8')

  test('presence observation declares its permission, so a consuming app inherits it', () => {
    expect(manifest).toContain(PRESENCE_PERMISSION)
  })

  test('the Bluetooth runtime permissions are still declared', () => {
    for (const permission of ['BLUETOOTH_SCAN', 'BLUETOOTH_CONNECT']) {
      expect(manifest).toContain(`android.permission.${permission}`)
    }
  })
})
