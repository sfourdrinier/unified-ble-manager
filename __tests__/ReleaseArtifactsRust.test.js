// __tests__/ReleaseArtifactsRust.test.js
//
// SBOM-Rust slice for UBM 5.0 (trackourhealth/bun-mono#1188; closes packaging
// open item 1, U-LICENSE/U8 gap): the generator merges the Cargo-resolved Rust
// workspace graph (`cargo metadata --locked --offline`, 201 nodes: 173 + the zbus stack for the BlueZ OS adapter + ubm-mobile) into
// SBOM.cdx.json / THIRD_PARTY_LICENSES.json. Cargo license evidence comes from
// declared metadata, except an exact reviewed license-file override where the
// vendored text establishes more specific terms.

const { execFileSync } = require('child_process')
const crypto = require('crypto')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { normalizeCargoSlashLicense } = require('../scripts/release/generate-dependency-artifacts')

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

const BTLEPLUG_PURL = 'pkg:cargo/btleplug@0.12.0'
const BTLEPLUG_LICENSE = 'BSD-3-Clause AND (MIT OR Apache-2.0)'
const BTLEPLUG_LICENSE_SHA256 = '95f1ea7e261c12c46fe8f67d2ddb7a92ebb1a5fd10d127e4ab3003f0701d9f56'

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
  test('the packed F01 proof binds every Cargo consumer build to the pinned rustc', () => {
    const proof = read('scripts/ci/f01-packed-dispatch-proof.js')
    expect(proof).toContain("run('rustup', ['which', '--toolchain', toolchain, 'rustc'])")
    expect(proof).toContain("['run', toolchain, 'cargo', 'build'")
    expect(proof).toContain("['run', toolchain, 'cargo', 'check'")
    expect(proof).toContain('RUSTC: rustc')
    expect(proof).toContain("process.platform === 'darwin'")
    expect(proof).toContain("'libubm5_napi_echo.dylib'")
    expect(proof).toContain("process.platform === 'win32'")
    expect(proof).toContain("'ubm5_napi_echo.dll'")
    expect(proof).not.toContain("const active = run('rustc', ['--version'])")
  })

  test('clean-checkout preflight binds Tauri Cargo calls to the pinned rustc', () => {
    const preflight = read('scripts/ci/preflight.sh')
    expect(preflight).toContain('rustup which --toolchain "$PINNED_TOOLCHAIN" rustc')
    expect(preflight).toContain('export RUSTC="$PINNED_RUSTC"')
    expect(preflight).toContain('rustup which --toolchain "$PINNED_TOOLCHAIN" rustdoc')
    expect(preflight).toContain('export RUSTDOC="$PINNED_RUSTDOC"')
    expect(preflight).toContain('PINNED_TOOLCHAIN_BIN="$(dirname "$PINNED_RUSTC")"')
    expect(preflight).toContain('export PATH="$PINNED_TOOLCHAIN_BIN:$PATH"')
    expect(preflight).toContain('rustup run "$PINNED_TOOLCHAIN" cargo fmt')
    expect(preflight).toContain('rustup run "$PINNED_TOOLCHAIN" cargo test')
    expect(preflight).toContain('rustup run "$PINNED_TOOLCHAIN" cargo clippy')
    expect(preflight).toContain('rustup run "$PINNED_TOOLCHAIN" cargo check')
  })

  test('canonicalizes only allowlisted nonempty legacy Cargo slash terms', () => {
    expect(normalizeCargoSlashLicense('MIT/Apache-2.0')).toBe('(MIT OR Apache-2.0)')
    expect(() => normalizeCargoSlashLicense('MIT/')).toThrow('Invalid slash-separated Cargo license declaration')
    expect(() => normalizeCargoSlashLicense('MIT/Unverified-License')).toThrow(
      'Invalid slash-separated Cargo license declaration'
    )
  })

  test('generator --check passes with the merged artifacts', () => {
    runGenerator(['--check'])
  })

  test('covers the full cargo graph with pkg:cargo purls and exact pinned versions', () => {
    const metadata = cargoMetadata()
    expect(metadata.packages).toHaveLength(201)

    const sbom = readJson('SBOM.cdx.json')
    const inventory = readJson('THIRD_PARTY_LICENSES.json')
    const cargoComponents = sbom.components.filter(component => component.purl.startsWith('pkg:cargo/'))
    expect(cargoComponents).toHaveLength(201)
    expect(new Set(cargoComponents.map(component => component['bom-ref'])).size).toBe(201)

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

  test('normalizes legacy slash declarations and retains reviewed btleplug license-file evidence', () => {
    const metadata = cargoMetadata()
    const declaredByPurl = new Map(
      metadata.packages.map(pkg => [cargoPurl(pkg.name, pkg.version), pkg.license || null])
    )
    const slashSeparated = [...declaredByPurl.entries()]
      .filter(([, license]) => license !== null && license.includes('/'))
      .map(([purl]) => purl)
      .sort()
    expect(slashSeparated).toHaveLength(11)

    const sbom = readJson('SBOM.cdx.json')
    const inventory = readJson('THIRD_PARTY_LICENSES.json')
    const cargoComponents = sbom.components.filter(component => component.purl.startsWith('pkg:cargo/'))

    for (const component of cargoComponents) {
      const declared = declaredByPurl.get(component.purl)
      const properties = Object.fromEntries(
        (component.properties || []).map(property => [property.name, property.value])
      )
      if (component.purl === BTLEPLUG_PURL) {
        expect(component.licenses).toEqual([{ expression: BTLEPLUG_LICENSE }])
        expect(properties['unified-ble-manager:license-source']).toBe('reviewed-cargo-license-file')
        expect(properties['unified-ble-manager:license-file']).toBe('vendor/btleplug/LICENSE.md')
        expect(properties['unified-ble-manager:license-review-required']).toBeUndefined()
      } else if (declared !== null && declared.includes('/')) {
        expect(component.licenses).toEqual([{ expression: `(${declared.split('/').join(' OR ')})` }])
        expect(properties['unified-ble-manager:license-review-required']).toBeUndefined()
      } else if (declared !== null) {
        // Declared SPDX expressions pass through verbatim — never rewritten.
        expect(component.licenses).toEqual([{ expression: declared }])
        expect(properties['unified-ble-manager:license-review-required']).toBeUndefined()
      }
      if (component.purl !== BTLEPLUG_PURL) {
        expect(properties['unified-ble-manager:license-source']).toMatch(/^cargo-manifest/)
      }
    }

    expect(inventory.unresolved).toEqual([])
    const inventoryByPurl = new Map(inventory.packages.map(entry => [entry.purl, entry]))
    const btleplug = inventoryByPurl.get(BTLEPLUG_PURL)
    expect(btleplug).toMatchObject({
      license: BTLEPLUG_LICENSE,
      licenseSource: 'reviewed-cargo-license-file',
      evidence: { fileName: 'vendor/btleplug/LICENSE.md' },
    })
    for (const purl of slashSeparated.filter(purl => purl !== BTLEPLUG_PURL)) {
      const declared = declaredByPurl.get(purl)
      expect(inventoryByPurl.get(purl)).toMatchObject({
        license: `(${declared.split('/').join(' OR ')})`,
        licenseSource: 'cargo-manifest-license-normalized',
        declared,
      })
    }
    expect(inventory.reviewedCargoOverrides).toContainEqual({
      dependency: 'btleplug@0.12.0',
      fileName: 'LICENSE.md',
      license: BTLEPLUG_LICENSE,
      sha256: BTLEPLUG_LICENSE_SHA256,
    })
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
