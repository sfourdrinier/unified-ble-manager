const fs = require('fs')
const path = require('path')
const { UNIFIED_BLE_IMPLEMENTATION_VERSION } = require('../src/implementation-version')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
const pkg = JSON.parse(read('package.json'))

describe('PR12 stable 4.0.0 qualification', () => {
  test('package identity is stable 4.0.0 without a prerelease suffix', () => {
    expect(pkg.version).toBe('4.0.0')
    expect(UNIFIED_BLE_IMPLEMENTATION_VERSION).toBe('4.0.0')
  })

  test('consumer docs publish 4.0.0 without promoting backend support labels', () => {
    const readme = read('README.md')
    const platforms = read('docs/PLATFORMS.md')
    const changelog = read('CHANGELOG.md')
    const support = read('docs/generated/PLATFORM_SUPPORT.md')

    expect(readme).toContain(pkg.version)
    expect(readme).toContain('Package SemVer and backend support labels are independent')
    expect(platforms).toContain('`unified-ble-manager@4.0.0` is the published **stable package/API**')
    expect(changelog).toContain('## [4.0.0]')
    expect(changelog).toMatch(/does not promote backend support labels/i)
    expect(support).toContain('unified-ble-manager@4.0.0')
    expect(support).toContain(
      'makes no Preview, Live Preview, Supported, or Reliability-qualified platform claim for the current package'
    )
    expect(support).toMatch(/Experimental|blocked|not bound to current package artifact/)
  })
})
