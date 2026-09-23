'use strict'

// D2 (finding 101): AGENTS.md describes the 5.x branch and no longer states
// that BlueZ uses a dbus-next dependency on the production path.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const agents = fs.readFileSync(path.join(root, 'AGENTS.md'), 'utf8')

describe('AGENTS.md 5.x contract', () => {
  test('title names the 5.x line', () => {
    expect(agents.split('\n')[0]).toMatch(/5\.x/)
    expect(agents.split('\n')[0]).not.toMatch(/4\.x/)
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
