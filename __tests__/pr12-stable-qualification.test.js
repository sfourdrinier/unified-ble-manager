const fs = require('fs')
const path = require('path')
const { UNIFIED_BLE_IMPLEMENTATION_VERSION } = require('../src/implementation-version')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
const pkg = JSON.parse(read('package.json'))

describe('PR12 package identity qualification', () => {
  test('package identity is the 5.0.0 release candidate', () => {
    expect(pkg.version).toBe('5.0.0-rc.1')
    expect(UNIFIED_BLE_IMPLEMENTATION_VERSION).toBe('5.0.0-rc.1')
  })

  test('consumer docs identify the stable source without inventing publication or backend support', () => {
    const readme = read('README.md')
    const platforms = read('docs/PLATFORMS.md')
    const changelog = read('CHANGELOG.md')
    const support = read('docs/generated/PLATFORM_SUPPORT.md')

    expect(readme).toContain(pkg.version)
    expect(readme).toContain('Package SemVer and backend support labels are')
    expect(readme).toContain('independent: each radio backend keeps its evidence-derived label')
    expect(platforms).toContain('it does not by itself prove npm publication or immutability')
    expect(platforms).toContain('backend support labels remain independent')
    expect(changelog).toContain('## [4.0.0]')
    expect(changelog).toContain('## [4.0.7]')
    expect(changelog).toContain('## [4.0.10]')
    expect(changelog).toContain('## [4.0.12]')
    expect(changelog).toContain('## [4.0.22]')
    expect(changelog).toMatch(/does not promote backend support labels/i)
    expect(support).toContain(`unified-ble-manager@${pkg.version}`)
    expect(support).toContain(
      'makes no Preview, Live Preview, Supported, or Reliability-qualified platform claim for the current package'
    )
    expect(support).toMatch(/Experimental|blocked|not bound to current package artifact/)
  })
})
