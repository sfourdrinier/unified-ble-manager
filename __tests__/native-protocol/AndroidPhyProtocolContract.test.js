const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(repositoryRoot, relativePath), 'utf8')
}

describe('Android PHY Native Protocol v2 contract', () => {
  test('adds PHY commands and result fields without reusing priority wire IDs', () => {
    const schema = JSON.parse(read('native/protocol/schema/native-protocol-v2.json'))
    const abi = JSON.parse(read('native/protocol/schema/native-protocol-v2-abi.json'))

    expect(schema.commandKinds).toContain('readPhy')
    expect(schema.commandKinds).toContain('requestPhy')
    expect(schema.resultKinds).toContain('phy')
    expect(schema.connectionPhys).toEqual(['le1m', 'le2m', 'leCoded'])
    expect(abi.enums.commandKinds.readPhy).toBeGreaterThan(abi.enums.commandKinds.requestPriority)
    expect(abi.enums.commandKinds.requestPhy).toBeGreaterThan(abi.enums.commandKinds.readPhy)
    expect(abi.enums.resultKinds.phy).toBeGreaterThan(abi.enums.resultKinds.priority)
    expect(abi.fields.command.phyTx).toBeGreaterThan(abi.fields.command.connectionPriority)
    expect(abi.fields.command.phyRx).toBeGreaterThan(abi.fields.command.phyTx)
    expect(abi.fields.result.phyTx).toBeGreaterThan(abi.fields.result.priorityAccepted)
    expect(abi.fields.result.phyRx).toBeGreaterThan(abi.fields.result.phyTx)
    expect(abi.fields.result.phyAccepted).toBeGreaterThan(abi.fields.result.phyRx)
  })

  test('generated protocol projections contain the PHY contract', () => {
    expect(read('src/native-protocol/generated/native-protocol-v2-schema.ts')).toContain("'readPhy'")
    expect(read('native/protocol/generated/NativeProtocolV2Schema.hpp')).toContain('readPhy')
    expect(read('android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/generated/NativeProtocolV2Schema.kt')).toContain(
      'READ_PHY'
    )
  })
})
