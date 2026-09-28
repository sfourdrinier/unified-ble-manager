'use strict'

// D2 (finding 101): AGENTS.md describes the 5.x branch and no longer states
// that BlueZ uses a dbus-next dependency on the production path.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const agents = fs.readFileSync(path.join(root, 'AGENTS.md'), 'utf8')

describe('AGENTS.md 5.x contract', () => {
  test('5.0 authority is current contracts and operational guidance, not the historical 4.0 plan', () => {
    const index = fs.readFileSync(path.join(root, 'docs/README.md'), 'utf8')
    const plan = fs.readFileSync(path.join(root, 'docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md'), 'utf8')
    expect(agents).toContain('docs/README.md#current-50-authority')
    expect(agents).not.toContain('Read `docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`')
    expect(index).toContain('## Current 5.0 authority')
    expect(index).toMatch(/UNIFIED_BLE_4\.0_IMPLEMENTATION_PLAN\.md[^\n]+\| Historical \|/)
    expect(plan.slice(0, 1800)).toContain('Historical architecture and migration record')
    expect(plan.slice(0, 1800)).toContain('not current 5.0 scope, sequencing, or release authority')
    for (const required of ['src/public/', 'src/backend-contract/', 'UNIFIED_SEMANTICS.md', 'NATIVE_ARTIFACTS.md', '../RELEASE.md'])
      expect(index).toContain(required)
  })

  test.each(['README.md', 'docs/BACKGROUND.md', 'docs/GETTING_STARTED.md', 'docs/BONDING.md', 'docs/FORK.md', 'docs/GAPS.4.0.md', 'docs/PERFORMANCE.md', 'docs/TVOS.md'])(
    '%s does not direct current implementation to the historical plan', relative => {
      const text = fs.readFileSync(path.join(root, relative), 'utf8')
      expect(text).not.toMatch(/(?:authority|contract|Contract|rules)[^\n]*\[`?(?:docs\/)?UNIFIED_BLE_4\.0_IMPLEMENTATION_PLAN/)
      expect(text).toContain('#current-50-authority')
    }
  )
  test('title names the 5.x line', () => {
    expect(agents.split('\n')[0]).toMatch(/5\.x/)
    expect(agents.split('\n')[0]).not.toMatch(/4\.x/)
  })

  test('automated review guidance names the same current contract line', () => {
    const review = fs.readFileSync(path.join(root, '.coderabbit.yaml'), 'utf8')
    expect(review).toContain('Contract violations against 5.x:')
    expect(review).not.toMatch(/Contract violations against [34]\.x:/)
  })

  test('no dbus-next-as-dependency statement remains', () => {
    for (const line of agents.split('\n')) {
      expect(line).not.toMatch(/optional.*dbus-next|dbus-next.*depend/)
    }
  })

  test('names the active React Native factory boundary, not the historical control module', () => {
    const hostImplementations = agents.split('## Host implementations')[1].split('## Evidence and support')[0]
    expect(hostImplementations).toContain('`UnifiedBleRustCore`')
    expect(hostImplementations).toContain('production factory')
    expect(hostImplementations).toMatch(/historical\s+`UnifiedBleProtocolControl`/)
    expect(hostImplementations).not.toContain('uses the versioned `UnifiedBleProtocolControl` boundary')
  })
})
