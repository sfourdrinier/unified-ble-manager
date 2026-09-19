'use strict'

// D3 (finding 101): dbus-next is not a consumer-facing dependency. The legacy
// D-Bus boundary stays only as an unreachable parity reference until Phase 4
// deletion, so it must not ship to consumers as a peer dependency. The
// package-surface suite already proves no entrypoint loads it.

const packageJson = require('../package.json')

describe('dbus-next ships to no consumer', () => {
  test('absent from peerDependencies', () => {
    expect(packageJson.peerDependencies ?? {}).not.toHaveProperty('dbus-next')
  })

  test('absent from peerDependenciesMeta', () => {
    expect(packageJson.peerDependenciesMeta ?? {}).not.toHaveProperty('dbus-next')
  })
})
