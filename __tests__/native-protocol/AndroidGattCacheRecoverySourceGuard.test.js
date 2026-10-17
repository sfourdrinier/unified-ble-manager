const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')
const radioPath = path.join(
  root,
  'android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'
)

describe('Android GATT cache recovery source guard', () => {
  test('uses supported reconnect and rediscovery instead of hidden cache-refresh reflection', () => {
    const source = fs.readFileSync(radioPath, 'utf8')

    expect(source).not.toMatch(/getMethod\s*\(\s*["']refresh["']\s*\)/)
    expect(source).not.toMatch(/\brefreshGatt\b/)
    const reflectedMethods = [...source.matchAll(/javaClass\.getMethod\(\s*"([^"]+)"/g)].map(match => match[1])
    expect(reflectedMethods).toEqual(['createBond'])
    const sourceWithoutAllowedBondReflection = source.replace('import java.lang.reflect.InvocationTargetException', '')
    expect(sourceWithoutAllowedBondReflection).not.toMatch(
      /\bgetDeclaredMethod\b|\bClass\.forName\b|\bjava\.lang\.reflect\b/i
    )
    expect(source).toContain('clearCharCacheForDevice(key)')
    expect(source).toContain('discovered.remove(key)')
    // The reconnect-after-teardown path keeps the caller's connect parameters
    // and the attempt identity used to correlate the replacement's own outcome.
    // Runtime ordering and cleanup are pinned by OwnedAndroidGattConnectOwnershipTest.
    expect(source).toContain('val queued = PendingConnect(autoConnect, phyMask, attempt)')
    expect(source).toContain('pendingReconnect[key] = queued')
    expect(source).toContain('openGatt(deviceId, key, pending.autoConnect, pending.phyMask, pending.attempt)')
  })
})
