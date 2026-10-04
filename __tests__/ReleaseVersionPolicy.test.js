const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const { execFileSync } = require('node:child_process')
const { runNapiArtifactPackagingProof } = require('../scripts/ci/check-napi-artifact-packaging')
const { classifyReleaseVersion, validateReleaseIdentity } = require('../scripts/release/release-version-policy')

describe('current UBM release version policy', () => {
  const packagePath = path.resolve(__dirname, '../package.json')
  const originalRead = fs.readFileSync.bind(fs)
  const manifest = JSON.parse(originalRead(packagePath, 'utf8'))

  afterEach(() => jest.restoreAllMocks())

  function runWithManifest(update) {
    jest.spyOn(console, 'log').mockImplementation(() => {})
    jest
      .spyOn(fs, 'readFileSync')
      .mockImplementation((file, ...options) =>
        file === packagePath ? JSON.stringify({ ...manifest, ...update }) : originalRead(file, ...options)
      )
    return runNapiArtifactPackagingProof()
  }

  const runWithVersion = version => runWithManifest({ version })

  test('accepting stable versions does not bypass the source export or license guards', () => {
    expect(() =>
      runWithManifest({ version: '5.0.0', exports: { ...manifest.exports, './napi': './bindings/napi/index.js' } })
    ).toThrow('must not ship dev-only subpath ./napi')
    jest.restoreAllMocks()
    expect(() =>
      runWithManifest({
        version: '5.0.0',
        files: manifest.files.filter(file => file !== 'LICENSE-UBM-SOURCE-AVAILABLE-1.0.md')
      })
    ).toThrow('files must include')
  })

  test.each(['5.0.0', '5.0.1', '5.1.0', '5.0.0-rc.16', '5.1.0-rc.0'])(
    'the real source packaging gate accepts %s without bypassing other assertions',
    version => expect(runWithVersion(version)).toBe(true)
  )

  test.each([
    '4.0.28',
    '6.0.0',
    '5.00.0',
    '5.0',
    'v5.0.0',
    '5.0.0-rc',
    '5.0.0-rc.01',
    '5.0.0-beta.1',
    '5.0.0+build',
    '5.0.0\n',
    '5.0.0-rc.16\n',
    '5.9007199254740992.0',
    undefined,
    null,
    5
  ])('the source packaging gate refuses malformed or unauthorized version %j', version =>
    expect(() => runWithVersion(version)).toThrow()
  )

  test.each(['5.0.0', '5.0.1', '5.1.0'])('stable %s selects latest', version => {
    expect(classifyReleaseVersion(version)).toEqual({ npmDistTag: 'latest', isStable: true })
  })

  test.each(['5.0.0-rc.16', '5.1.0-rc.0'])('authorized candidate %s selects next', version => {
    expect(classifyReleaseVersion(version)).toEqual({ npmDistTag: 'next', isStable: false })
  })

  test('release identity requires the canonical package and exact immutable version tag', () => {
    expect(validateReleaseIdentity({ name: 'unified-ble-manager', version: '5.0.0' }, 'v5.0.0')).toEqual({
      npmDistTag: 'latest',
      isStable: true
    })
    expect(() => validateReleaseIdentity({ name: 'unified-ble-manager', version: '5.0.0' }, 'v5.0.1')).toThrow(
      'does not match'
    )
    expect(() => validateReleaseIdentity({ name: 'another-package', version: '5.0.0' }, 'v5.0.0')).toThrow(
      'canonical package'
    )
  })
})

describe('actual publication channel command', () => {
  test.each([
    ['5.0.0', 'v5.0.0', 'unified-ble-manager', 'latest', true],
    ['5.0.1', 'v5.0.1', 'unified-ble-manager', 'latest', true],
    ['5.0.0-rc.16', 'v5.0.0-rc.16', 'unified-ble-manager', 'next', false],
    ['5.0.0', 'v5.0.1', 'unified-ble-manager', undefined, undefined],
    ['5.0.0', 'v5.0.0', 'another-package', undefined, undefined],
    ['5.0.0-rc.01', 'v5.0.0-rc.01', 'unified-ble-manager', undefined, undefined]
  ])('version %s / tag %s / package %s', (version, tag, name, distTag, stable) => {
    const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-release-channel-'))
    try {
      const script = path.join(fixture, 'scripts/release/release-version-policy.js')
      fs.mkdirSync(path.dirname(script), { recursive: true })
      fs.copyFileSync(path.resolve(__dirname, '../scripts/release/release-version-policy.js'), script)
      fs.writeFileSync(path.join(fixture, 'package.json'), JSON.stringify({ name, version }))
      const envFile = path.join(fixture, 'environment')
      const outputFile = path.join(fixture, 'output')
      fs.writeFileSync(envFile, 'preserved\n')
      fs.writeFileSync(outputFile, 'preserved\n')
      const run = () =>
        execFileSync(process.execPath, [script], {
          env: { ...process.env, GITHUB_REF_NAME: tag, GITHUB_ENV: envFile, GITHUB_OUTPUT: outputFile },
          stdio: 'pipe'
        })
      if (distTag === undefined) {
        expect(run).toThrow()
        expect(fs.readFileSync(envFile, 'utf8')).toBe('preserved\n')
        expect(fs.readFileSync(outputFile, 'utf8')).toBe('preserved\n')
      } else {
        expect(run().toString()).toContain(`Publishing ${name}@${version}`)
        expect(fs.readFileSync(envFile, 'utf8')).toBe(`preserved\nNPM_DIST_TAG=${distTag}\n`)
        expect(fs.readFileSync(outputFile, 'utf8')).toBe(`preserved\nis_stable=${stable}\n`)
      }
    } finally {
      fs.rmSync(fixture, { recursive: true, force: true })
    }
  })
})
