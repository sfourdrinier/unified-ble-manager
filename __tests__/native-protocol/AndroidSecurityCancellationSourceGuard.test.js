const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../..')

describe('Android pairing cancellation source guard', () => {
  test('does not remove native bond ownership when supported Android APIs cannot cancel bonding', () => {
    const radio = fs.readFileSync(
      path.join(repositoryRoot, 'android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'),
      'utf8'
    )
    const security = fs.readFileSync(
      path.join(repositoryRoot, 'src/backends/reactnative/react-native-android-security.ts'),
      'utf8'
    )

    expect(radio).toMatch(
      /internal fun clearPendingBondPair\(deviceId: String\): Boolean =\s*throw UnsupportedOperationException\(/
    )
    expect(radio).toContain('Android bonding cancellation is unsupported before API 37')
    expect(radio).not.toContain('cancelBondProcess')
    expect(radio).not.toContain('pendingBondPairs.remove(deviceId.uppercase()) !== null')
    expect(security).not.toContain('cleanupPairing(nativePeerId)')
    expect(security).toContain("android.security.pair.cancellation")
  })
})
