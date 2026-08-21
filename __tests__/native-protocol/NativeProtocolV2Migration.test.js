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
