const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../../..')

describe('Android PHY source seam', () => {
  test('routes PHY operations through the per-device serial queue and current-GATT quarantine', () => {
    const source = fs.readFileSync(
      path.join(repositoryRoot, 'android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'),
      'utf8'
    )

    expect(source).toContain('fun readPhy(')
    expect(source).toContain('fun requestPhy(')
    expect(source).toContain('override fun onPhyRead(')
    expect(source).toContain('override fun onPhyUpdate(')
    expect(source).toContain('gatt.readPhy()')
    expect(source).toContain('gatt.setPreferredPhy(')
    expect(source).toContain('isCurrentGattCallback(gatt)')
    expect(source).toContain('deviceQueues.remove(key)?.clear()')
  })
})
