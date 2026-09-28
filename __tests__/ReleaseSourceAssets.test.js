const { execFileSync } = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { readBluezSourceAsset } = require('../scripts/release/generate-dependency-artifacts')

const root = path.resolve(__dirname, '..')

test('bundled BlueZ source extension has its own licenses and never claims to be a linked daemon', () => {
  const output = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-source-license-test-'))
  try {
    execFileSync(process.execPath, ['scripts/release/generate-dependency-artifacts.js', '--output-directory', output], {
      cwd: root,
      stdio: 'pipe'
    })
    const sbom = JSON.parse(fs.readFileSync(path.join(output, 'SBOM.cdx.json'), 'utf8'))
    const inventory = JSON.parse(fs.readFileSync(path.join(output, 'THIRD_PARTY_LICENSES.json'), 'utf8'))
    const component = sbom.components.find(entry => entry.purl === 'pkg:generic/bluez-ubm-le-gatt-source@5.87')
    expect(component).toBeDefined()
    expect(component.type).toBe('file')
    expect(component.licenses).toEqual([{ expression: 'GPL-2.0-or-later AND LGPL-2.1-or-later' }])
    expect(component.properties).toEqual(
      expect.arrayContaining([
        { name: 'unified-ble-manager:distribution', value: 'source-only-daemon-extension' },
        { name: 'unified-ble-manager:linked-native-code', value: 'false' },
        { name: 'unified-ble-manager:packaged-daemon-binary', value: 'false' }
      ])
    )
    const licensed = inventory.packages.find(entry => entry.purl === component.purl)
    expect(licensed.license).toBe('GPL-2.0-or-later AND LGPL-2.1-or-later')
    expect(licensed.evidence.licenseFiles).toHaveLength(2)
  } finally {
    fs.rmSync(output, { recursive: true, force: true })
  }
})

test.each([
  ['version', 'Unreviewed BlueZ source-asset identity or license'],
  ['license', 'Unreviewed BlueZ source-asset identity or license'],
  ['distribution', 'BlueZ source-asset distribution requires a new review'],
  ['path', 'Invalid BlueZ source evidence path or hash'],
  ['patch-hash', 'BlueZ source evidence changed: ubm-le-gatt-5.87.patch'],
  ['license-file', 'BlueZ source evidence changed: COPYING']
])(
  'source-asset validation fails closed on %s drift',
  (change, expectedError) => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-source-evidence-test-'))
    try {
      fs.cpSync(path.join(root, 'vendor/bluez'), directory, { recursive: true })
      const manifestPath = path.join(directory, 'source-asset-manifest.json')
      const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
      if (change === 'version') manifest.version = '5.88'
      if (change === 'license') manifest.licenseExpression = 'LicenseRef-UBM-Source-Available-1.0'
      if (change === 'distribution') manifest.distribution.linkedIntoPackageNativeLibraries = true
      if (change === 'path') manifest.patch.file = '../ubm-le-gatt-5.87.patch'
      if (change === 'patch-hash') manifest.patch.sha256 = '0'.repeat(64)
      if (change === 'license-file') fs.appendFileSync(path.join(directory, 'COPYING'), 'changed')
      fs.writeFileSync(manifestPath, JSON.stringify(manifest))
      expect(() => readBluezSourceAsset(directory)).toThrow(expectedError)
    } finally {
      fs.rmSync(directory, { recursive: true, force: true })
    }
  }
)
