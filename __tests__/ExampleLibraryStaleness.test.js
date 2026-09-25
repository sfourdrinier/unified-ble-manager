// __tests__/ExampleLibraryStaleness.test.js
//
// The Expo example consumes this package as a `file:..` dependency, which pnpm
// COPIES at install time. Three things can therefore be stale in that copy, and
// they fail in three different ways:
//
//   - the sealed native identity: fails loudly at runtime with
//     protocol.incompatible, which is correct and already covered;
//   - the embedded RustCore the pod compiles against: same, once the app is
//     rebuilt from a fresh copy;
//   - the built `lib/` output: fails SILENTLY. The app cannot resolve the
//     package, never finishes loading, and nothing anywhere says so. That is
//     the failure this guard exists to name.
//
// build-tv.sh already verifies the staged copy for the TV path (finding 176).
// The phone path had no equivalent until finding 241.

'use strict'

const fs = require('node:fs')
const path = require('node:path')

const guardPath = path.join(__dirname, '..', 'examples-shared', 'dev', 'verify-example-library.js')
const { inspectExampleLibrary, describeLibraryOutcome } = require(guardPath)
const { prepareExampleIos } = require('../examples-shared/dev/prepare-example-ios')

const ROOT = path.join(__dirname, '..')

function repoFacts(overrides = {}) {
  return {
    identity: 'sourceDigest: abc',
    version: '5.0.0-rc.10',
    ...overrides
  }
}

function copyFacts(overrides = {}) {
  return {
    present: true,
    identity: 'sourceDigest: abc',
    version: '5.0.0-rc.10',
    hasBuiltLib: true,
    ...overrides
  }
}

describe('example library staleness guard', () => {
  test('a copy that matches the repo and carries its build output is usable', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts() })
    expect(outcome.ok).toBe(true)
    expect(outcome.state).toBe('current')
  })

  test('a missing copy is named, not treated as current', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts({ present: false }) })
    expect(outcome.ok).toBe(false)
    expect(outcome.state).toBe('absent')
    expect(describeLibraryOutcome(outcome)).toMatch(/install/i)
  })

  test('a stale sealed identity is refused before the app fails closed at runtime', () => {
    const outcome = inspectExampleLibrary({
      repo: repoFacts(),
      copy: copyFacts({ identity: 'sourceDigest: stale' })
    })
    expect(outcome.ok).toBe(false)
    expect(outcome.state).toBe('stale-identity')
    expect(describeLibraryOutcome(outcome)).toMatch(/protocol\.incompatible/)
  })

  test('a copy with no built lib is the SILENT failure and must be named explicitly', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts({ hasBuiltLib: false }) })
    expect(outcome.ok).toBe(false)
    expect(outcome.state).toBe('no-build-output')
    const message = describeLibraryOutcome(outcome)
    expect(message).toMatch(/lib/)
    expect(message).toMatch(/prepack/)
  })

  test('a version mismatch is refused', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts({ version: '4.0.28' }) })
    expect(outcome.ok).toBe(false)
    expect(outcome.state).toBe('version-mismatch')
    expect(describeLibraryOutcome(outcome)).toContain('4.0.28')
  })

  test('the refresh advice names the larger heap the copy needs', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts({ identity: 'other' }) })
    expect(describeLibraryOutcome(outcome)).toContain('max-old-space-size')
  })
})

describe('the Expo example runs the guard before it builds a native host', () => {
  const expo = JSON.parse(fs.readFileSync(path.join(ROOT, 'example-expo', 'package.json'), 'utf8'))

  test('iOS prepares its copy and generated configuration; Android retains its copy guard', () => {
    expect(expo.scripts.ios).toContain('prepare-example-ios.js')
    expect(expo.scripts.ios).toContain('verify-expo-ios-restoration.js')
    expect(expo.scripts.android).toContain('verify-example-library.js')
  })

  test('Apple CI exercises both guards and triggers when they change', () => {
    const workflow = fs.readFileSync(path.join(ROOT, '.github', 'workflows', 'apple-ci.yml'), 'utf8')
    const changes = fs.readFileSync(path.join(ROOT, '.github', 'workflows', 'ci.yml'), 'utf8')
    const install = workflow.indexOf('pnpm --dir example-expo install --no-frozen-lockfile')
    const peerAlign = workflow.indexOf('npx expo install --fix')
    const prepare = workflow.indexOf('node examples-shared/dev/prepare-example-ios.js example-expo')
    const prebuild = workflow.indexOf('npx expo prebuild --clean --no-install --platform ios')
    const restoration = workflow.indexOf('node examples-shared/dev/verify-expo-ios-restoration.js example-expo')
    expect(prepare).toBeGreaterThan(install)
    expect(prepare).toBeGreaterThan(peerAlign)
    expect(restoration).toBeGreaterThan(prebuild)
    expect(changes).toContain("- 'examples-shared/dev/**'")
  })
})

describe('iOS example preparation', () => {
  test('a fresh installed copy needs no package refresh', () => {
    const steps = []
    prepareExampleIos({
      verifyRootIdentity: () => {},
      ensureApple: () => steps.push('apple'),
      inspectCopy: () => ({ ok: true, state: 'current' }),
      verifyCopyApple: () => steps.push('verify-apple'),
      refreshCopy: () => steps.push('refresh')
    })
    expect(steps).toEqual(['apple', 'verify-apple'])
  })

  test('a stale generated root identity stops the build before it trusts the copy', () => {
    const steps = []
    expect(() => prepareExampleIos({
      verifyRootIdentity: () => { throw new Error('generated identity is stale') },
      ensureApple: () => steps.push('apple'),
      inspectCopy: () => ({ ok: true, state: 'current' }),
      verifyCopyApple: () => steps.push('verify-apple'),
      refreshCopy: () => steps.push('refresh')
    })).toThrow(/generated identity is stale/)
    expect(steps).toEqual([])
  })

  test('a copy with current JavaScript identity but stale RustCore is refreshed and verified', () => {
    const steps = []
    let stale = true
    prepareExampleIos({
      verifyRootIdentity: () => {},
      ensureApple: () => steps.push('apple'),
      inspectCopy: () => ({ ok: true, state: 'current' }),
      verifyCopyApple: () => {
        steps.push('verify-apple')
        if (stale) throw new Error('stale RustCore')
      },
      refreshCopy: () => {
        steps.push('refresh')
        stale = false
      }
    })
    expect(steps).toEqual(['apple', 'verify-apple', 'refresh', 'verify-apple'])
  })

  test('a stale library copy refreshes once and fails closed if still stale', () => {
    const steps = []
    expect(() => prepareExampleIos({
      verifyRootIdentity: () => {},
      ensureApple: () => steps.push('apple'),
      inspectCopy: () => ({ ok: false, state: 'stale-identity' }),
      verifyCopyApple: () => steps.push('verify-apple'),
      refreshCopy: () => steps.push('refresh')
    })).toThrow(/stale-identity/)
    expect(steps).toEqual(['apple', 'refresh'])
  })
})
