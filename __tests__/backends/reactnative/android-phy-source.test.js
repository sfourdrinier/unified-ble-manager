const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../../..')

describe('Android PHY source seam', () => {
  test('routes PHY operations through the per-device serial queue and current-GATT quarantine', () => {
    const source = fs.readFileSync(
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

    expect(source).toContain('fun readPhy(')
    expect(source).toContain('fun requestPhy(')
    expect(source).toContain('override fun onPhyRead(')
    expect(source).toContain('override fun onPhyUpdate(')
    expect(source).toContain('Result.failure(IllegalStateException("onPhyUpdate status=$status"))')
    expect(source).toContain('gatt.readPhy()')
    expect(source).toContain('gatt.setPreferredPhy(')
    expect(dispatcher).toContain('phyMaskValue(command.optionalString(17))')
    expect(dispatcher).toContain('phyMaskValue(command.optionalString(18))')
    expect(source).toContain('isCurrentGattCallback(gatt)')
    expect(source).toContain('deviceQueues.remove(key)?.clear()')
  })

  test('reports runtime PHY capability from the Android API level and guards dispatcher entrypoints', () => {
    const control = fs.readFileSync(
      path.join(
        repositoryRoot,
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
      ),
      'utf8'
    )
    const dispatcher = fs.readFileSync(
      path.join(
        repositoryRoot,
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
      ),
      'utf8'
    )

    expect(control).toContain('phyAvailable')
    expect(control).toContain('Build.VERSION.SDK_INT >= Build.VERSION_CODES.O')
    expect(dispatcher).toContain('Build.VERSION.SDK_INT >= Build.VERSION_CODES.O')
    expect(dispatcher).toContain('readPhy')
    expect(dispatcher).toContain('requestPhy')
  })
})
