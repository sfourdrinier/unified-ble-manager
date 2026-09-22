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

  // The repository presents the UBM Source Available License as its license.
  // GitHub, npm and most tooling classify a repository from the top-level
  // LICENSE file, so that file must be the UBM text and nothing else — a root
  // Apache text reads as "this repository is Apache-2.0", which it is not.
  test('the top-level LICENSE is the UBM Source Available License, not Apache', () => {
    expect(exists('LICENSE')).toBe(true)
    expect(read('LICENSE')).toBe(read(SAL_LICENSE_FILE))
    expect(read('LICENSE')).not.toContain('Apache License')
    expect(read('LICENSE')).not.toContain('TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION')
  })

  // Material retained from the Apache baseline stays under its Apache-2.0
  // grant, and Apache-2.0 section 4(a) requires that recipients receive a copy
  // of that license. The text therefore still ships — under LICENSES/, where it
  // cannot be mistaken for the repository's license.
  test('the retained Apache-2.0 text ships under LICENSES/, never at the repository root', () => {
    const apachePath = 'LICENSES/Apache-2.0.txt'
    expect(exists(apachePath)).toBe(true)
    expect(read(apachePath)).toContain('Apache License')
    expect(read(apachePath)).toContain('TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION')
    const packageJson = require('../package.json')
    expect(packageJson.files).toContain(apachePath)
    const rootLicenseFiles = fs
      .readdirSync(root, { withFileTypes: true })
      .filter(entry => entry.isFile() && /^(LICEN[CS]E|COPYING)/i.test(entry.name))
      .map(entry => entry.name)
      .filter(name => /Apache License/.test(read(name)))
    expect(rootLicenseFiles).toEqual([])
  })

  test('NOTICE points retained Apache material at LICENSES/Apache-2.0.txt and names LICENSE as the UBM license', () => {
    const notice = read(NOTICE_FILE)
    expect(notice).toContain('LICENSES/Apache-2.0.txt')
    expect(notice).not.toMatch(/Apache License 2\.0 grant; see LICENSE\./)
  })

  // The H10 simulator is new 5.0 material, so it is under the UBM license like
  // every sibling crate — not MIT — and, like them, it is never published.
  test('the H10 simulator is UBM-licensed and unpublishable, like its siblings', () => {
    const manifest = read('tool/h10-sim/Cargo.toml')
    expect(manifest).not.toMatch(/^license\s*=\s*"MIT"/m)
    expect(manifest).toMatch(/^license-file\s*=\s*"\.\.\/\.\.\/LICENSE-UBM-SOURCE-AVAILABLE-1\.0\.md"/m)
    expect(manifest).toMatch(/^publish\s*=\s*false/m)
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
      stdio: 'pipe'
    })
    const sbom = JSON.parse(read('SBOM.cdx.json'))
    expect(sbom.metadata.component.licenses).toEqual([{ expression: SAL_LICENSE_REF }])
    const generator = read('scripts/release/generate-dependency-artifacts.js')
    expect(generator).toContain(SAL_LICENSE_REF)
  })

  test('GOVERNANCE describes the project as source-available and points at NOTICE, not open source', () => {
    const governance = read('GOVERNANCE.md')
    expect(governance).toMatch(/source-available/)
    expect(governance).toContain(NOTICE_FILE)
    expect(governance).not.toMatch(/open-source/)
  })

  test('live self-descriptions present the package as source-available, never as open source', () => {
    const support = read('SUPPORT.md')
    expect(support).toContain('Support is best-effort maintenance.')
    expect(support).not.toMatch(/open-source/i)

    const threat = read('docs/security/UNIFIED_BLE_4.0_THREAT_MODEL.md')
    expect(threat).toMatch(/source-available, multi-host/)
    expect(threat).toContain('NOTICE')
    expect(threat).not.toMatch(/open-source/i)

    const plan = read('docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
    expect(plan).toContain('new source-available package with no production users')
    expect(plan).toContain('a source-available release process')
    expect(plan).toContain('designed for the public ecosystem')
    expect(plan).toContain('## 21. Public product quality')
    expect(plan).toContain('proves the source-available package independently')
    expect(plan).not.toMatch(/open-source (package|library|foundation|release process|product quality|ecosystem)/)
    expect(plan).not.toMatch(/open-source, multi-host/)
    expect(plan).not.toMatch(/new open-source package/)
  })

  test('dated records keep their history behind a superseding license note', () => {
    const roadmap = read('ROADMAP.4.0.md')
    expect(roadmap).toContain('new open-source package line')
    expect(roadmap).toContain('License note (5.0)')
    expect(roadmap).toContain('NOTICE')

    const adr = read('docs/ADR/2026-07-4.0-open-source-governance.md')
    expect(adr).toContain('open-source foundation')
    expect(adr).toContain('License note (5.0)')
    expect(adr).toContain('NOTICE')
    expect(exists('docs/ADR/2026-07-4.0-open-source-governance.md')).toBe(true)
  })

  test('RELEASE release invariants name the dual-license documents, not only the root LICENSE', () => {
    const release = read('RELEASE.md')
    expect(release).toContain(NOTICE_FILE)
    expect(release).toContain(SAL_LICENSE_FILE)
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
