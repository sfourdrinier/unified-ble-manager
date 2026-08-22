const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

describe('PR8 documentation contract', () => {
  test('documents the controls facade, truthful capabilities, bounded operations, and GATT recovery', () => {
    const migration = read('MIGRATION_4.0.md')
    const semantics = read('docs/UNIFIED_SEMANTICS.md')
    const documents = `${migration}\n${semantics}`

    expect(migration).not.toContain('connection:request-att-mtu')
    expect(migration).not.toMatch(/connection\.(readRssi|requestMtu)\b/)
    expect(migration).toContain('connection.controls.readRssi')
    expect(migration).toContain('connection.controls.requestMtu')
    expect(migration).toContain('connection.controls.maximumWriteLength')
    expect(migration).toContain("connection.rediscoverGatt({ reason: 'service-changed' })")
    expect(migration).toContain("connection.rediscoverGatt({ reason: 'manual' })")

    for (const id of [
      'connection:rssi',
      'connection:effective-mtu',
      'connection:request-mtu',
      'connection:priority',
      'connection:parameters',
      'connection:phy',
      'connection:subrate',
      'gatt:maximum-write-length',
      'gatt:write-without-response-readiness'
    ]) {
      expect(documents).toContain(`\`${id}\``)
    }

    for (const field of [
      'state',
      'connectionGeneration',
      'observedAtMonotonicMs',
      'source',
      'authority',
      'limitations'
    ]) {
      expect(semantics).toContain(`\`${field}\``)
    }
    expect(semantics).toMatch(/request(?:ed)?[^\n]*observation|observation[^\n]*request(?:ed)?/i)
    expect(semantics).toMatch(/one serialized queue per physical\s+connection/)
    expect(semantics).toContain('bounded')
    expect(semantics).toMatch(/Different connection\s+queues may proceed concurrently/)
    expect(semantics).toContain('gatt:write-without-response-readiness')
    expect(semantics).toMatch(/write-without-response\s+readiness[\s\S]{0,200}unsupported[\s\S]{0,200}advertises/i)
    expect(documents).toContain("reason: 'service-changed' | 'manual'")
    expect(documents).toContain('BluetoothGatt.refresh()')
    expect(documents).toMatch(/uncertain write/i)
    expect(documents).toMatch(/deterministic|host compile/i)
    expect(documents).toMatch(
      /not[\s\S]{0,120}physical-radio[\s\S]{0,120}qualification|physical-radio[\s\S]{0,120}not[\s\S]{0,120}qualification/i
    )
  })
})
