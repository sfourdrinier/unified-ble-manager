// __tests__/AndroidUnitTestScope.test.js
//
// The Android gate must run the module's whole JVM unit-test suite. A
// `--tests` filter once limited it to the legacy dispatcher, so the Rust-core
// adapter and presence tests never ran in CI and a broken test went unseen.

const fs = require('node:fs')
const path = require('node:path')

const pkg = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'package.json'), 'utf8'))

describe('Android JVM unit-test gate scope', () => {
  it('runs every unified-ble-manager unit test, not a filtered subset', () => {
    const script = pkg.scripts['test:native-protocol:android']
    expect(script).toContain(':unified-ble-manager:testDebugUnitTest')
    expect(script).not.toContain('--tests')
  })
})
