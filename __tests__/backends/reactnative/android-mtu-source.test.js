const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../../..')

describe('Android effective MTU source seam', () => {
  test('uses generation-bound cached GATT state through the serial queue', () => {
    const radio = fs.readFileSync(
      path.join(repositoryRoot, 'android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'),
      'utf8'
    )
    const dispatcher = fs.readFileSync(
      path.join(
        repositoryRoot,
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
      ),
      'utf8'
    )

    expect(radio).toContain('fun readEffectiveMtu(')
    expect(radio).toContain('effectiveMtuByDevice')
    expect(radio).toContain('override fun onMtuChanged(')
    expect(radio).toContain('isCurrentGattCallback(gatt)')
    expect(radio).toContain('deviceQueues.remove(key)?.clear()')
    expect(dispatcher).toContain('"readMtu" -> readMtu(command)')
    expect(dispatcher).toContain('radio.readEffectiveMtu(')
  })
})
