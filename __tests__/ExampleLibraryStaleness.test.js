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
const os = require('node:os')
const { writeBuildFingerprint } = require('../scripts/release/generate-build-fingerprint')

const guardPath = path.join(__dirname, '..', 'examples-shared', 'dev', 'verify-example-library.js')
const { inspectExampleLibrary, describeLibraryOutcome, readRepoFacts, readCopyFacts } = require(guardPath)
const { prepareExampleIos } = require('../examples-shared/dev/prepare-example-ios')

const ROOT = path.join(__dirname, '..')

function repoFacts(overrides = {}) {
  return {
    identity: 'sourceDigest: abc',
    version: '5.0.0-rc.10',
    buildFingerprint: 'current-build',
    ...overrides
  }
}

function copyFacts(overrides = {}) {
  return {
    present: true,
    identity: 'sourceDigest: abc',
    version: '5.0.0-rc.10',
    hasBuiltLib: true,
    buildFingerprint: 'current-build',
    ...overrides
  }
}

describe('example library staleness guard', () => {
  test('unchanged package/native identity cannot conceal an older JavaScript build', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts({ buildFingerprint: 'old-build' }) })
    expect(outcome).toMatchObject({ ok: false, state: 'stale-build' })
    expect(describeLibraryOutcome(outcome)).toMatch(/prepack/)
  })

  test.each(['repo', 'copy'])('missing or invalid %s seal fails closed', side => {
    const repo = repoFacts(side === 'repo' ? { buildFingerprint: null, buildError: 'Build seal is missing' } : {})
    const copy = copyFacts(side === 'copy' ? { buildFingerprint: null, buildError: 'Build seal is missing' } : {})
    const outcome = inspectExampleLibrary({ repo, copy })
    expect(outcome).toMatchObject({ ok: false, state: side === 'repo' ? 'stale-root-build' : 'invalid-copy-build' })
    expect(describeLibraryOutcome(outcome)).toContain('Build seal is missing')
  })
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

  test('copy refresh guidance preserves the lockfile and uses supported force recopy', () => {
    const outcome = inspectExampleLibrary({ repo: repoFacts(), copy: copyFacts({ identity: 'other' }) })
    const advice = describeLibraryOutcome(outcome)
    expect(advice).toContain('install-example-dependencies.js <example> --force --frozen-lockfile')
    expect(advice).not.toContain('rm -rf')
    expect(advice).not.toContain('--no-frozen-lockfile')
  })
})

describe('canonical build seals for repository and packed example copies', () => {
  let root
  let example
  let copyRoot
  function write(relative, contents) {
    const target = path.join(root, relative)
    fs.mkdirSync(path.dirname(target), { recursive: true })
    fs.writeFileSync(target, contents)
  }
  beforeEach(() => {
    root = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-example-build-'))
    example = path.join(root, 'example-expo')
    copyRoot = path.join(example, 'node_modules', 'unified-ble-manager')
    write('package.json', JSON.stringify({ name: 'unified-ble-manager', version: '5.0.0-rc.10' }))
    write('src/generated/native-build-identity.ts', "export const identity = { sourceDigest: 'abcdef0123456789' }\n")
    write('src/supervisor.ts', 'export const retry = false\n')
    write('scripts/dev-only.js', '// omitted by packaging\n')
    write('lib/module/index.js', 'export const retry = false\n')
    writeBuildFingerprint(root)
    for (const relative of ['package.json', 'src', 'lib']) {
      fs.mkdirSync(copyRoot, { recursive: true })
      fs.cpSync(path.join(root, relative), path.join(copyRoot, relative), { recursive: true })
    }
  })
  afterEach(() => fs.rmSync(root, { recursive: true, force: true }))
  const inspect = () => inspectExampleLibrary({ repo: readRepoFacts(root), copy: readCopyFacts(example) })

  test('a packed copy omitting checkout-only inputs validates against the current repository seal', () => {
    expect(fs.existsSync(path.join(copyRoot, 'scripts'))).toBe(false)
    expect(inspect()).toMatchObject({ ok: true, state: 'current' })
  })
  test('source changes require rebuilding root before an old or newly copied package can be current', () => {
    write('src/supervisor.ts', 'export const retry = true\n')
    const outcome = inspect()
    expect(outcome).toMatchObject({ ok: false, state: 'stale-root-build' })
    expect(describeLibraryOutcome(outcome)).toContain('src/supervisor.ts')
    writeBuildFingerprint(root)
    expect(inspect()).toMatchObject({ ok: false, state: 'stale-build' })
  })
  test.each(['missing', 'invalid-json', 'tampered'])('a %s copied seal cannot report current', kind => {
    const target = path.join(copyRoot, 'lib', 'ubm-build-fingerprint.json')
    if (kind === 'missing') fs.unlinkSync(target)
    else if (kind === 'invalid-json') fs.writeFileSync(target, '{')
    else {
      const seal = JSON.parse(fs.readFileSync(target, 'utf8'))
      seal.fingerprint = 'tampered'
      fs.writeFileSync(target, JSON.stringify(seal))
    }
    expect(inspect()).toMatchObject({ ok: false, state: 'invalid-copy-build' })
  })
  test('a missing repository seal fails closed even when the copied seal remains valid', () => {
    fs.unlinkSync(path.join(root, 'lib', 'ubm-build-fingerprint.json'))
    expect(inspect()).toMatchObject({ ok: false, state: 'stale-root-build' })
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
    const install = workflow.indexOf('node examples-shared/dev/install-example-dependencies.js example-expo --no-frozen-lockfile')
    const peerAlign = workflow.indexOf('node examples-shared/dev/install-example-dependencies.js --expo-fix example-expo')
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
  test.each(['stale-build', 'stale-root-build', 'invalid-copy-build'])(
    '%s triggers one refresh and revalidation',
    state => {
      const steps = []
      let refreshed = false
      prepareExampleIos({
        verifyRootIdentity: () => {},
        ensureApple: () => steps.push('apple'),
        inspectCopy: () => {
          steps.push('inspect')
          return refreshed ? { ok: true, state: 'current' } : { ok: false, state }
        },
        verifyCopyApple: () => steps.push('verify-apple'),
        refreshCopy: () => {
          steps.push('refresh')
          refreshed = true
        }
      })
      expect(steps).toEqual(['apple', 'inspect', 'refresh', 'inspect', 'verify-apple'])
    }
  )
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
    expect(() =>
      prepareExampleIos({
        verifyRootIdentity: () => {
          throw new Error('generated identity is stale')
        },
        ensureApple: () => steps.push('apple'),
        inspectCopy: () => ({ ok: true, state: 'current' }),
        verifyCopyApple: () => steps.push('verify-apple'),
        refreshCopy: () => steps.push('refresh')
      })
    ).toThrow(/generated identity is stale/)
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
    expect(() =>
      prepareExampleIos({
        verifyRootIdentity: () => {},
        ensureApple: () => steps.push('apple'),
        inspectCopy: () => ({ ok: false, state: 'stale-identity' }),
        verifyCopyApple: () => steps.push('verify-apple'),
        refreshCopy: () => steps.push('refresh')
      })
    ).toThrow(/stale-identity/)
    expect(steps).toEqual(['apple', 'refresh'])
  })
})
