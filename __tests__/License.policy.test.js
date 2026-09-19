// __tests__/License.policy.test.js
//
// Contract cases for the UBM 5.0 license adoption (trackourhealth/bun-mono#1188).
//
// The 11 scenarios below are the policy table from the UBM 5.0 licensing
// decision. Each fixture carries a FIXED classification agreed in that
// decision. These are documentation/policy guards, not a runtime
// person-counting implementation: nothing here counts people, phones home, or
// enforces entitlements, and no source module may gain such behavior to
// satisfy these tests.

const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

const SAL_LICENSE_FILE = 'LICENSE-UBM-SOURCE-AVAILABLE-1.0.md'

const FREE = 'free'
const COMMERCIAL_LICENSE_REQUIRED = 'commercial-license-required'
const EXISTING_RIGHTS_UNAFFECTED = 'existing-rights-unaffected'

// Classified fixtures: scenario, fixed outcome, and the operative license
// phrases that establish it. `document` names the file that must carry the
// evidence (the SAL text for new material, NOTICE for the baseline pointer).
const policyCases = [
  {
    scenario: 'qualifying open-source project of any company size, including revenue-generating projects',
    expected: FREE,
    document: SAL_LICENSE_FILE,
    evidence: [
      'Open-source projects:',
      'regardless of company size or whether the project earns revenue',
    ],
  },
  {
    scenario: 'closed-source commercial use by a company with one or two people in total',
    expected: FREE,
    document: SAL_LICENSE_FILE,
    evidence: ['Small companies:', 'no more than two people in total'],
  },
  {
    scenario: 'closed-source commercial use by a company with three or more people in total',
    expected: COMMERCIAL_LICENSE_REQUIRED,
    document: SAL_LICENSE_FILE,
    evidence: [
      'requires a separate written commercial license',
      'stephane@fourdrinier.com',
    ],
  },
  {
    scenario: 'noncommercial use',
    expected: FREE,
    document: SAL_LICENSE_FILE,
    evidence: ['Noncommercial use is also permitted'],
  },
  {
    scenario: 'large company with only one or two people working on its UBM project',
    expected: COMMERCIAL_LICENSE_REQUIRED,
    document: SAL_LICENSE_FILE,
    evidence: [
      'Company size is the number of distinct people',
      'Include staff working on unrelated projects',
    ],
  },
  {
    scenario: 'two developers on UBM integration plus another employee on an unrelated product',
    expected: COMMERCIAL_LICENSE_REQUIRED,
    document: SAL_LICENSE_FILE,
    evidence: [
      'regardless of role or whether they work on the project using UBM',
      'Include staff working on unrelated projects',
    ],
  },
  {
    scenario: 'employee plus contractor plus working founder in the company',
    expected: COMMERCIAL_LICENSE_REQUIRED,
    document: SAL_LICENSE_FILE,
    evidence: ['working founders, individual contractors and other active workers'],
  },
  {
    scenario: 'one-person company plus many AI agents',
    expected: FREE,
    document: SAL_LICENSE_FILE,
    evidence: ['automated tools and AI agents are not counted'],
  },
  {
    scenario: 'ordinary users run a lawfully distributed app',
    expected: FREE,
    document: SAL_LICENSE_FILE,
    evidence: ['An ordinary end user may run an application lawfully distributed under this license'],
  },
  {
    scenario: 'public wrapper around a closed-source commercial service operated by a company over two people',
    expected: COMMERCIAL_LICENSE_REQUIRED,
    document: SAL_LICENSE_FILE,
    evidence: [
      'Making only a sample, wrapper or SDK public does not make a separate closed-source commercial product a qualifying open-source project',
    ],
  },
  {
    scenario: 'existing historical Apache copy',
    expected: EXISTING_RIGHTS_UNAFFECTED,
    document: SAL_LICENSE_FILE,
    evidence: ['does not revoke rights already granted under another license'],
  },
]

describe('UBM 5.0 license policy contract cases', () => {
  test('fixtures are static classifications, not runtime counting logic', () => {
    expect(policyCases).toHaveLength(11)
    for (const policyCase of policyCases) {
      expect([FREE, COMMERCIAL_LICENSE_REQUIRED, EXISTING_RIGHTS_UNAFFECTED]).toContain(policyCase.expected)
      expect(policyCase.evidence.length).toBeGreaterThan(0)
      // JSON-serializable: no functions, no computed outcomes.
      expect(() => JSON.parse(JSON.stringify(policyCase))).not.toThrow()
      for (const value of Object.values(policyCase)) {
        expect(typeof value === 'function').toBe(false)
      }
    }
  })

  test('the SAL authority file carries the selected license identity and contact', () => {
    const text = read(SAL_LICENSE_FILE)
    expect(text).toContain('UBM Source Available License 1.0')
    expect(text).toContain('LicenseRef-UBM-Source-Available-1.0')
    expect(text).toContain('Stephane Fourdrinier')
    expect(text).toContain('stephane@fourdrinier.com')
  })

  test('the SAL text is presented as source-available, never as OSI-approved open source', () => {
    const text = read(SAL_LICENSE_FILE)
    expect(text).toContain('not an OSI-approved open-source license')
  })

  test('the SAL text authorizes no runtime enforcement machinery', () => {
    const text = read(SAL_LICENSE_FILE)
    expect(text).toContain('There is no revenue threshold')
    expect(text).toContain('telemetry requirement or online license-check requirement')
    expect(text).toContain('Nothing here requires a device, sensor or application to perform an automatic runtime shutdown')
  })

  test.each(policyCases.map(policyCase => [policyCase.scenario, policyCase]))(
    'policy case: %s',
    (_scenario, policyCase) => {
      const document = read(policyCase.document)
      for (const phrase of policyCase.evidence) {
        expect(document).toContain(phrase)
      }
    }
  )

  test('a company of two is inside the free grant and three triggers licensing for nonqualifying commercial use', () => {
    const text = read(SAL_LICENSE_FILE)
    expect(text).toContain('no more than two people in total')
    const freeCompanyCases = policyCases.filter(policyCase => policyCase.expected === FREE)
    const licenseRequiredCases = policyCases.filter(
      policyCase => policyCase.expected === COMMERCIAL_LICENSE_REQUIRED
    )
    expect(freeCompanyCases.length).toBeGreaterThan(0)
    expect(licenseRequiredCases.length).toBeGreaterThan(0)
  })
})
