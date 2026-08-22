const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(repositoryRoot, relativePath), 'utf8')
}

describe('Android effective MTU Native Protocol v2 contract', () => {
  test('adds a read probe and a distinct effective-MTU result field', () => {
    const schema = JSON.parse(read('native/protocol/schema/native-protocol-v2.json'))
    const abi = JSON.parse(read('native/protocol/schema/native-protocol-v2-abi.json'))

    expect(schema.commandKinds).toContain('readMtu')
    expect(schema.resultKinds).toContain('mtu')
    expect(abi.enums.commandKinds.readMtu).toBeGreaterThan(abi.enums.commandKinds.requestPhy)
    expect(abi.fields.result.effectiveMtu).toBeGreaterThan(abi.fields.result.phyAccepted)
    expect(abi.fields.result.effectiveMtu).not.toBe(abi.fields.result.negotiatedMtu)
  })

  test('generated projections contain the Android probe without changing host behavior', () => {
    expect(read('src/native-protocol/generated/native-protocol-v2-schema.ts')).toContain("'readMtu'")
    expect(read('native/protocol/generated/NativeProtocolV2Schema.hpp')).toContain('readMtu')
    expect(read('android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/generated/NativeProtocolV2Schema.kt')).toContain(
      'READ_MTU'
    )
    expect(read('ios/Generated/NativeProtocolV2Schema.swift')).toContain('readMtu')
  })
})
