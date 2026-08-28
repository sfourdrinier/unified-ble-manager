const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

test('Android ABI-6 bonded peers bridge uses the owned adapter and connect permission', () => {
  const radio = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt')
  const dispatcher = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt')

  expect(radio).toContain('data class BondedPeerSnapshot')
  expect(radio).toContain('getBondedDevices()')
  expect(radio).toContain('Manifest.permission.BLUETOOTH_CONNECT')
  expect(dispatcher).toContain('"enumerateBondedPeers"')
  expect(dispatcher).toContain('"bondedPeers"')
})
