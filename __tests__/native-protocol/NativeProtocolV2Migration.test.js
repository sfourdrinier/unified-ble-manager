const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

function filesUnder(relativeDirectory) {
  const absoluteDirectory = path.join(root, relativeDirectory)
  const entries = fs.readdirSync(absoluteDirectory, { withFileTypes: true })
  return entries.flatMap((entry) => {
    const relativePath = path.join(relativeDirectory, entry.name)
    return entry.isDirectory() ? filesUnder(relativePath) : [relativePath]
  })
}

describe('native protocol v2 migration', () => {
  test('requires ABI 7 and preserves every previously assigned wire ID', () => {
    const schema = JSON.parse(read('native/protocol/schema/native-protocol-v2.json'))
    const manifest = JSON.parse(read('native/protocol/schema/native-protocol-v2-abi.json'))
    expect(schema.abiVersion).toBe(7)
    expect(manifest.version).toBe(7)
    expect(manifest.recordKinds).toEqual(expect.objectContaining({
      attachment: 1,
      connectionPath: 2,
      databasePath: 3,
      restorationAdoptionResult: 19,
      scanOptions: 20,
      adapterStateSnapshot: 23,
      bondedPeerSnapshot: 24
    }))
    expect(manifest.enums.commandKinds).toEqual(expect.objectContaining({ requestPriority: 20, enumerateBondedPeers: 24 }))
    expect(manifest.enums.resultKinds).toEqual(expect.objectContaining({ priority: 18, phy: 19, bondedPeers: 20 }))
    expect(manifest.fields.command).toEqual(expect.objectContaining({ pairTransport: 19, connectionIntent: 20 }))
    expect(manifest.fields.result).toEqual(expect.objectContaining({ effectiveMtu: 22, bondedPeers: 23 }))
  })

  test('ships one v2 schema/generator/runtime authority without the v1 stack', () => {
    expect(fs.existsSync(path.join(root, 'native/protocol/schema/native-protocol-v2.json'))).toBe(true)
    expect(fs.existsSync(path.join(root, 'native/protocol/schema/native-protocol-v2-abi.json'))).toBe(true)
    expect(read('scripts/native-protocol/generate-native-protocol.js')).toContain('native-protocol-v2')

    const productionRoots = ['src', 'native', 'android', 'ios', 'scripts', '__tests__']
    const v1References = []
    for (const productionRoot of productionRoots) {
      for (const relative of filesUnder(productionRoot)) {
        const normalizedRelative = relative.split(path.sep).join('/')
        if (normalizedRelative === '__tests__/native-protocol/NativeProtocolV2Migration.test.js') continue
        if (!/\.(?:cpp|hpp|h|kt|swift|mm|ts|js|json)$/.test(relative)) continue
        const content = read(relative)
        if (/NativeProtocolV1|native-protocol-v1|__unifiedBleNativeProtocolV1|native_protocol::v1|native_protocol\/v1/.test(content)) {
          v1References.push(relative)
        }
      }
    }
    expect(v1References).toEqual([])
  })
})
