// __tests__/License.packaging.test.js
//
// Package-content guards for the UBM 5.0 license adoption
// (trackourhealth/bun-mono#1188): new SAL-licensed material must ship coherent
// metadata (SEE LICENSE IN + license-file pointers + packed license files),
// while legitimate inherited Apache notices keep passing.

const fs = require('fs')
const path = require('path')
const { execFileSync } = require('child_process')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
const exists = relativePath => fs.existsSync(path.join(root, relativePath))

const SAL_LICENSE_FILE = 'LICENSE-UBM-SOURCE-AVAILABLE-1.0.md'
const NOTICE_FILE = 'NOTICE'
const CONTRIBUTION_TERMS_FILE = 'UBM-CONTRIBUTION-TERMS-1.0.md'
const SAL_LICENSE_REF = 'LicenseRef-UBM-Source-Available-1.0'
const APACHE_BASELINE_COMMIT = '8c8195dd0430ff847d9492ce32f4e63ea3a5df1d'

describe('UBM 5.0 license packaging and metadata', () => {
  test('package.json uses the supported SEE LICENSE IN form for the custom license', () => {
    const packageJson = require('../package.json')
    expect(packageJson.license).toBe(`SEE LICENSE IN ${SAL_LICENSE_FILE}`)
  })

  test('packed files include the SAL text, NOTICE, and contribution terms', () => {
    const packageJson = require('../package.json')
    expect(packageJson.files).toContain(SAL_LICENSE_FILE)
    expect(packageJson.files).toContain(NOTICE_FILE)
    expect(packageJson.files).toContain(CONTRIBUTION_TERMS_FILE)
    expect(exists(SAL_LICENSE_FILE)).toBe(true)
    expect(exists(NOTICE_FILE)).toBe(true)
    expect(exists(CONTRIBUTION_TERMS_FILE)).toBe(true)
  })

  test('the existing Apache LICENSE is preserved for inherited 4.x material', () => {
    expect(exists('LICENSE')).toBe(true)
    expect(read('LICENSE')).toContain('Apache License')
    expect(read('LICENSE')).toContain('TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION')
  })

  test('NOTICE names the confirmed licensor, the Apache baseline, and the material statement', () => {
    const notice = read(NOTICE_FILE)
    expect(notice).toContain('Stephane Fourdrinier')
    expect(notice).toContain(APACHE_BASELINE_COMMIT)
    expect(notice).toContain(SAL_LICENSE_REF)
    expect(notice).toContain('stephane@fourdrinier.com')
    expect(notice).toMatch(/retained|inherited/i)
    expect(notice).toMatch(/new|5\.0/)
  })

  test('native Tauri manifest points at the license file without a fabricated SPDX identifier', () => {
    const manifest = read('native/tauri/Cargo.toml')
    expect(manifest).not.toMatch(/^license\s*=/m)
    expect(manifest).not.toContain('LicenseRef')
    expect(manifest).toContain(`license-file = "../../${SAL_LICENSE_FILE}"`)
    const licenseFilePath = path.join(root, 'native', 'tauri', '..', '..', SAL_LICENSE_FILE)
    expect(fs.existsSync(licenseFilePath)).toBe(true)
  })

  test('contribution terms record the rights grant and CONTRIBUTING requires explicit assent', () => {
    const terms = read(CONTRIBUTION_TERMS_FILE)
    expect(terms).toContain('UBM 5.0 contribution terms 1.0')
    expect(terms).toContain('You retain ownership of your contribution')
    expect(terms).toContain('UBM Source Available License 1.0')

    const contributing = read('CONTRIBUTING.md')
    expect(contributing).toContain(CONTRIBUTION_TERMS_FILE)
    expect(contributing).toContain('assent')
    expect(contributing).toContain('contributor')
    expect(contributing).toContain('contribution')
    expect(contributing).toContain('terms version')
    expect(contributing).toContain('DCO')
    expect(contributing).toMatch(/DCO[^.]*not[^.]*assent/i)
    // The old Apache default is retained for 4.x material, not silently replaced.
    expect(contributing).toContain('Apache-2.0')
    expect(contributing).toMatch(/4\.x/)
  })

  test('tarball verifier allowlists the new license artifacts', () => {
    const verifier = read('scripts/ci/verify-package-tarballs.js')
    expect(verifier).toContain(`package/${SAL_LICENSE_FILE}`)
    expect(verifier).toContain(`package/${NOTICE_FILE}`)
    expect(verifier).toContain(`package/${CONTRIBUTION_TERMS_FILE}`)
  })

  test('SBOM reports the LicenseRef identifier through the real generator', () => {
    execFileSync(process.execPath, ['scripts/release/generate-dependency-artifacts.js', '--check'], {
      cwd: root,
      stdio: 'pipe',
    })
    const sbom = JSON.parse(read('SBOM.cdx.json'))
    expect(sbom.metadata.component.licenses).toEqual([{ expression: SAL_LICENSE_REF }])
    const generator = read('scripts/release/generate-dependency-artifacts.js')
    expect(generator).toContain(SAL_LICENSE_REF)
  })

  test('README presents the custom license as source-available, not open source', () => {
    const readme = read('README.md')
    const licenseSection = readme.slice(readme.indexOf('## License'))
    expect(licenseSection).toContain(SAL_LICENSE_FILE)
    expect(licenseSection).toContain('LICENSE')
    expect(licenseSection).toContain(NOTICE_FILE)
    expect(licenseSection).toMatch(/not.*OSI-approved|source-available/i)
    expect(licenseSection).not.toMatch(/licensed under the Apache License 2\.0/i)
  })
})
