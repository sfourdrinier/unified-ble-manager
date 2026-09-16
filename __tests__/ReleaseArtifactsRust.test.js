// __tests__/ReleaseArtifactsRust.test.js
//
// SBOM-Rust slice for UBM 5.0 (trackourhealth/bun-mono#1188; closes packaging
// open item 1, U-LICENSE/U8 gap): the generator merges the Cargo-resolved Rust
// workspace graph (`cargo metadata --locked --offline`, 172 nodes) into
// SBOM.cdx.json / THIRD_PARTY_LICENSES.json. License evidence is DECLARED
// metadata only — never fabricated, never guessed: ambiguous declarations
// stay NOASSERTION with a review flag.

const { execFileSync } = require('child_process')
const crypto = require('crypto')
const fs = require('fs')
const os = require('os')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
const readJson = relativePath => JSON.parse(read(relativePath))

const SAL_LICENSE_REF = 'LicenseRef-UBM-Source-Available-1.0'
const SAL_LICENSE_FILE = 'LICENSE-UBM-SOURCE-AVAILABLE-1.0.md'

function runGenerator(args) {
  execFileSync(process.execPath, ['scripts/release/generate-dependency-artifacts.js', ...args], {
    cwd: root,
    stdio: 'pipe',
  })
}

// Independent oracle: the real Rust generator, not the script under test.
function cargoMetadata() {
  const raw = execFileSync('cargo', ['metadata', '--locked', '--offline', '--format-version', '1'], {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  return JSON.parse(raw)
}

function cargoPurl(name, version) {
  return `pkg:cargo/${encodeURIComponent(name)}@${encodeURIComponent(version)}`
}

function npmPurl(name, version) {
  const encodedName = name
    .split('/')
    .map(segment => encodeURIComponent(segment))
    .join('/')
  return `pkg:npm/${encodedName}@${encodeURIComponent(version)}`
}

// Ambiguous legacy `/`-separated declarations: recorded as NOASSERTION with a
// review flag, never reinterpreted as OR/AND.
const EXPECTED_NOASSERTION_PURLS = [
  'pkg:cargo/btleplug@0.12.0',
  'pkg:cargo/cesu8@1.1.0',
  'pkg:cargo/dbus-tokio@0.7.6',
  'pkg:cargo/dbus@0.9.12',
  'pkg:cargo/jni@0.19.0',
  'pkg:cargo/libdbus-sys@0.2.7',
  'pkg:cargo/minimal-lexical@0.2.1',
  'pkg:cargo/plain@0.2.3',
  'pkg:cargo/same-file@1.0.6',
  'pkg:cargo/siphasher@1.0.3',
  'pkg:cargo/walkdir@2.5.0',
]

// Direct Rust dependencies pinned by docs/5.0.0-PACKAGING.md §6.
const EXPECTED_DIRECT_CARGO_PURLS = [
  'pkg:cargo/btleplug@0.12.0',
  'pkg:cargo/futures-util@0.3.34',
  'pkg:cargo/jni@0.22.4',
  'pkg:cargo/napi-build@2.4.2',
  'pkg:cargo/napi-derive@2.16.13',
  'pkg:cargo/napi@2.16.17',
  'pkg:cargo/tokio@1.53.1',
  'pkg:cargo/uniffi@0.32.1',
  'pkg:cargo/uuid@1.26.1',
  'pkg:cargo/wasm-bindgen@0.2.128',
]

const EXPECTED_WORKSPACE_PURLS = [
  'pkg:cargo/ubm-core@0.1.0',
  'pkg:cargo/ubm-desktop@0.1.0',
  'pkg:cargo/ubm5_jni_echo@0.1.0',
  'pkg:cargo/ubm5_napi_echo@0.1.0',
  'pkg:cargo/ubm5_uniffi_echo@0.1.0',
  'pkg:cargo/ubm5_wasm_echo@0.1.0',
]

// Frozen npm production inventory: the merge must not churn it.
const EXPECTED_NPM_PACKAGES = [
  { name: '@babel/runtime', version: '7.29.7', license: 'MIT' },
  { name: '@isaacs/fs-minipass', version: '4.0.1', license: 'ISC' },
  { name: 'abbrev', version: '4.0.0', license: 'ISC' },
  { name: 'chownr', version: '3.0.0', license: 'BlueOak-1.0.0' },
  { name: 'env-paths', version: '2.2.1', license: 'MIT' },
  { name: 'exponential-backoff', version: '3.1.2', license: 'Apache-2.0' },
  { name: 'fdir', version: '6.5.0', license: 'MIT' },
  { name: 'graceful-fs', version: '4.2.11', license: 'ISC' },
  { name: 'isexe', version: '4.0.0', license: 'BlueOak-1.0.0' },
  { name: 'minipass', version: '7.1.3', license: 'BlueOak-1.0.0' },
  { name: 'minizlib', version: '3.1.0', license: 'MIT' },
  { name: 'node-addon-api', version: '8.9.0', license: 'MIT' },
  { name: 'node-gyp', version: '12.4.0', license: 'MIT' },
  { name: 'nopt', version: '9.0.0', license: 'ISC' },
  { name: 'picomatch', version: '4.0.5', license: 'MIT' },
  { name: 'proc-log', version: '6.1.0', license: 'ISC' },
  { name: 'semver', version: '7.8.5', license: 'ISC' },
  { name: 'tar', version: '7.5.22', license: 'BlueOak-1.0.0' },
  { name: 'tinyglobby', version: '0.2.17', license: 'MIT' },
  { name: 'undici', version: '6.28.0', license: 'MIT' },
  { name: 'which', version: '6.0.1', license: 'ISC' },
  { name: 'yallist', version: '5.0.0', license: 'BlueOak-1.0.0' },
]

describe('SBOM Rust workspace merge (UBM 5.0)', () => {
  test('generator --check passes with the merged artifacts', () => {
    runGenerator(['--check'])
  })

  test('covers the full cargo graph with pkg:cargo purls and exact pinned versions', () => {
    const metadata = cargoMetadata()
    expect(metadata.packages).toHaveLength(172)

    const sbom = readJson('SBOM.cdx.json')
    const inventory = readJson('THIRD_PARTY_LICENSES.json')
    const cargoComponents = sbom.components.filter(component => component.purl.startsWith('pkg:cargo/'))
    expect(cargoComponents).toHaveLength(172)
    expect(new Set(cargoComponents.map(component => component['bom-ref'])).size).toBe(172)

    const expectedPurls = new Set(metadata.packages.map(pkg => cargoPurl(pkg.name, pkg.version)))
    expect(new Set(cargoComponents.map(component => component.purl))).toEqual(expectedPurls)

    // Exact versions: every component matches its Cargo.lock pin.
    const lockVersions = new Map()
    for (const pkg of metadata.packages) lockVersions.set(`${pkg.name}@${pkg.version}`, pkg.version)
    for (const component of cargoComponents) {
      expect(lockVersions.get(`${component.name}@${component.version}`)).toBe(component.version)
    }

    // The inventory covers exactly the same merged set as the SBOM.
    expect(inventory.packages).toHaveLength(sbom.components.length)
    expect(new Set(inventory.packages.map(entry => entry.purl))).toEqual(
      new Set(sbom.components.map(component => component.purl))
    )
  })

  test('records the documented direct Rust dependencies at their resolved versions', () => {
    const sbom = readJson('SBOM.cdx.json')
    const purls = new Set(sbom.components.map(component => component.purl))
    for (const purl of EXPECTED_DIRECT_CARGO_PURLS) {
      expect(purls.has(purl)).toBe(true)
    }
  })

  test('license evidence is declared-only; ambiguous stays NOASSERTION with a review flag', () => {
    const metadata = cargoMetadata()
    const declaredByPurl = new Map(
      metadata.packages.map(pkg => [cargoPurl(pkg.name, pkg.version), pkg.license || null])
    )
    const ambiguous = [...declaredByPurl.entries()]
      .filter(([, license]) => license !== null && license.includes('/'))
      .map(([purl]) => purl)
      .sort()
    expect(ambiguous).toEqual(EXPECTED_NOASSERTION_PURLS)

    const sbom = readJson('SBOM.cdx.json')
    const inventory = readJson('THIRD_PARTY_LICENSES.json')
    const cargoComponents = sbom.components.filter(component => component.purl.startsWith('pkg:cargo/'))

    for (const component of cargoComponents) {
      const declared = declaredByPurl.get(component.purl)
      const properties = Object.fromEntries(
        (component.properties || []).map(property => [property.name, property.value])
      )
      if (EXPECTED_NOASSERTION_PURLS.includes(component.purl)) {
        expect(component.licenses).toEqual([{ name: 'NOASSERTION' }])
        expect(properties['unified-ble-manager:license-review-required']).toBe('true')
      } else if (declared !== null) {
        // Declared SPDX expressions pass through verbatim — never rewritten.
        expect(component.licenses).toEqual([{ expression: declared }])
        expect(properties['unified-ble-manager:license-review-required']).toBeUndefined()
      }
      expect(properties['unified-ble-manager:license-source']).toMatch(/^cargo-manifest/)
    }

    // The inventory carries the same NOASSERTION set as its review list.
    expect(inventory.unresolved.map(entry => entry.purl).sort()).toEqual(EXPECTED_NOASSERTION_PURLS)
    for (const entry of inventory.unresolved) {
      expect(entry.reason).toMatch(/ambigu/i)
      expect(typeof entry.declared).toBe('string')
    }
    const inventoryByPurl = new Map(inventory.packages.map(entry => [entry.purl, entry]))
    for (const purl of EXPECTED_NOASSERTION_PURLS) {
      expect(inventoryByPurl.get(purl).license).toBe('NOASSERTION')
      expect(inventoryByPurl.get(purl).reviewRequired).toBe(true)
    }
  })

  test('workspace crates report the SAL LicenseRef with an extracted-text pointer', () => {
    const sbom = readJson('SBOM.cdx.json')
    const inventory = readJson('THIRD_PARTY_LICENSES.json')
    const byPurl = new Map(sbom.components.map(component => [component.purl, component]))

    for (const purl of EXPECTED_WORKSPACE_PURLS) {
      const component = byPurl.get(purl)
      expect(component).toBeDefined()
      expect(component.licenses).toEqual([{ expression: SAL_LICENSE_REF }])
      const properties = Object.fromEntries(
        (component.properties || []).map(property => [property.name, property.value])
      )
      expect(properties['unified-ble-manager:license-source']).toBe('cargo-manifest-license-file')
      expect(properties['unified-ble-manager:license-file']).toBe(SAL_LICENSE_FILE)
      const entry = inventory.packages.find(candidate => candidate.purl === purl)
      expect(entry.license).toBe(SAL_LICENSE_REF)
    }

    const generator = read('scripts/release/generate-dependency-artifacts.js')
    expect(generator).not.toMatch(/reviewedLicenseOverrides\[.cargo|license.*override.*cargo/i)
  })

  test('the npm production pipeline has no churn', () => {
    const sbom = readJson('SBOM.cdx.json')
    const inventory = readJson('THIRD_PARTY_LICENSES.json')

    const npmComponents = sbom.components.filter(component => component.purl.startsWith('pkg:npm/'))
    expect(npmComponents.map(component => ({
      name: component.group ? `${component.group}/${component.name}` : component.name,
      version: component.version,
      license: component.licenses,
    }))).toEqual(
      EXPECTED_NPM_PACKAGES.map(entry => ({
        name: entry.name,
        version: entry.version,
        license: [{ expression: entry.license }],
      }))
    )
    for (const component of npmComponents) {
      const values = (component.properties || []).map(property => property.value)
      expect(values.some(value => String(value).startsWith('cargo'))).toBe(false)
    }

    const npmInventory = inventory.packages.filter(entry => entry.purl.startsWith('pkg:npm/'))
    expect(npmInventory).toEqual(
      EXPECTED_NPM_PACKAGES.map(entry => ({
        name: entry.name,
        version: entry.version,
        license: entry.license,
        licenseSource: 'package-metadata',
        purl: npmPurl(entry.name, entry.version),
      }))
    )

    // The root dependency edge still fans out to the npm production roots only.
    const rootPurl = `pkg:npm/unified-ble-manager@${require('../package.json').version}`
    const rootEntry = sbom.dependencies.find(entry => entry.ref === rootPurl)
    expect(rootEntry.dependsOn).toEqual([
      'pkg:npm/%40babel/runtime@7.29.7',
      'pkg:npm/node-addon-api@8.9.0',
      'pkg:npm/node-gyp@12.4.0',
    ])
  })

  test('merged generation is byte-stable and leaks no local paths', () => {
    const temporaryRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-rust-artifacts-'))
    const first = path.join(temporaryRoot, 'first')
    const second = path.join(temporaryRoot, 'second')
    fs.mkdirSync(first)
    fs.mkdirSync(second)

    try {
      for (const outputDirectory of [first, second]) {
        runGenerator(['--output-directory', outputDirectory])
      }

      for (const fileName of ['SBOM.cdx.json', 'THIRD_PARTY_LICENSES.json']) {
        const firstBytes = fs.readFileSync(path.join(first, fileName))
        const secondBytes = fs.readFileSync(path.join(second, fileName))
        expect(crypto.createHash('sha256').update(firstBytes).digest('hex')).toBe(
          crypto.createHash('sha256').update(secondBytes).digest('hex')
        )
        const text = firstBytes.toString('utf8')
        expect(text).not.toContain('/Users/')
        expect(text).not.toContain('/home/')
        expect(text).not.toContain('node_modules/.pnpm')
        expect(text).not.toContain('path+file://')
        // Regeneration into the repo matches the committed bytes exactly.
        expect(firstBytes.toString('utf8')).toBe(read(fileName))
      }
    } finally {
      fs.rmSync(temporaryRoot, { recursive: true, force: true })
    }
  })
})
