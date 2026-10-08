'use strict'

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '..')
const script = path.join(root, 'example-expo/scripts/build-tv.sh')

test('the existing Apple lane compiles the complete consumer from its packed native artifact', () => {
  const workflow = fs.readFileSync(path.join(root, '.github/workflows/apple-ci.yml'), 'utf8')
  const lane = workflow.slice(workflow.indexOf('  tvos-library:'))
  expect(lane).toContain('uses: ./.github/actions/setup-js-package')
  expect(lane).toContain('pnpm native:apple:prepare')
  expect(lane).toContain('pnpm pack --pack-destination')
  expect(lane).toContain('TV_PACKAGE_TARBALL=')
  expect(lane).toContain('bash scripts/ci/check-tvos-packed-consumer.sh')
  expect(lane.indexOf('pnpm native:apple:prepare')).toBeLessThan(lane.indexOf('pnpm pack --pack-destination'))
  expect(lane.indexOf('pnpm pack --pack-destination')).toBeLessThan(lane.indexOf('bash scripts/ci/check-tvos-packed-consumer.sh'))
})

test('the cached React Native TV archive is verified before CocoaPods consumes it', () => {
  const { verifyTvArchive } = require('../example-expo/scripts/verify-tv-archive')
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-archive-test-'))
  try {
    const archive = path.join(fixture, 'archive.tgz')
    const digest = `${archive}.sha1`
    fs.writeFileSync(archive, 'abc')
    fs.writeFileSync(digest, 'a9993e364706816aba3e25717850c26c9cd0d89d\n')
    expect(() => verifyTvArchive(archive, digest)).not.toThrow()
    fs.writeFileSync(archive, 'altered')
    expect(() => verifyTvArchive(archive, digest)).toThrow('digest mismatch')
    fs.writeFileSync(digest, 'not a digest')
    expect(() => verifyTvArchive(archive, digest)).toThrow('invalid SHA-1')
  } finally {
    fs.rmSync(fixture, { recursive: true, force: true })
  }
})

test('packed TV staging uses the tarball and only stage-local shared sources', () => {
  const stage = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-packed-test-'))
  const tarball = path.join(stage, '..', `${path.basename(stage)}.tgz`)
  fs.writeFileSync(tarball, 'fixture: staging does not unpack the install input')
  try {
    const result = spawnSync('bash', [script, 'stage'], {
      encoding: 'utf8',
      env: { ...process.env, TV_STAGE_DIR: stage, TV_PACKAGE_TARBALL: tarball }
    })
    expect(result.status).toBe(0)
    const pkg = JSON.parse(fs.readFileSync(path.join(stage, 'package.json'), 'utf8'))
    expect(pkg.packageManager).toBe(JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8')).packageManager)
    expect(pkg.dependencies['unified-ble-manager']).toBe(`file:${tarball}`)
    expect(pkg.dependencies['react-native']).toBe('npm:react-native-tvos@0.86.3-0')
    expect(fs.readFileSync(path.join(stage, 'metro.config.js'), 'utf8')).toContain(
      "path.resolve(projectRoot, '.ubm-reference-shared')"
    )
    for (const name of ['shared.ts', 'headless-continuation-job.ts', 'headless-continuation-history.ts']) {
      const source = fs.readFileSync(path.join(stage, 'src/driver', name), 'utf8')
      expect(source).not.toContain('../../../examples-shared')
      expect(source).toContain('../../.ubm-reference-shared/driver/')
    }
    expect(fs.existsSync(path.join(stage, '.ubm-reference-shared/driver/index.ts'))).toBe(true)
    expect(fs.existsSync(path.join(stage, 'native/ios/ReferenceContinuationModule.swift'))).toBe(true)
    expect(fs.existsSync(path.join(stage, 'native/android/ReferenceContinuationModule.kt'))).toBe(true)
  } finally {
    fs.rmSync(stage, { recursive: true, force: true })
    fs.rmSync(tarball, { force: true })
  }
}, 120000)

test('packed input must exist before staging mutates the destination', () => {
  const stage = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-packed-test-'))
  try {
    fs.writeFileSync(path.join(stage, 'sentinel'), 'preserve')
    const result = spawnSync('bash', [script, 'stage'], {
      encoding: 'utf8',
      env: { ...process.env, TV_STAGE_DIR: stage, TV_PACKAGE_TARBALL: path.join(stage, 'absent.tgz') }
    })
    expect(result.status).not.toBe(0)
    expect(fs.readFileSync(path.join(stage, 'sentinel'), 'utf8')).toBe('preserve')
  } finally {
    fs.rmSync(stage, { recursive: true, force: true })
  }
})

test('a packed input inside the disposable stage is rejected without deleting it', () => {
  const stage = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-packed-test-'))
  try {
    const tarball = path.join(stage, 'release.tgz')
    fs.writeFileSync(tarball, 'must survive')
    const result = spawnSync('bash', [script, 'stage'], {
      encoding: 'utf8',
      env: { ...process.env, TV_STAGE_DIR: stage, TV_PACKAGE_TARBALL: tarball }
    })
    expect(result.status).not.toBe(0)
    expect(fs.readFileSync(tarball, 'utf8')).toBe('must survive')
  } finally {
    fs.rmSync(stage, { recursive: true, force: true })
  }
})

test('full TV target build keeps both maintained slices ARM64 and never refreshes packed native bytes', () => {
  const source = fs.readFileSync(script, 'utf8')
  const build = source.slice(source.indexOf('cmd_build_target() {'), source.indexOf('cmd_metro() {'))
  expect(build).toContain('cmd_verify_identity')
  expect(build).toContain('if [[ -z "${TV_PACKAGE_TARBALL:-}" ]]')
  expect(build).toContain('ARCHS=arm64 ONLY_ACTIVE_ARCH=YES CODE_SIGNING_ALLOWED=NO build')
  expect(source).toContain("build-simulator) cmd_build_target 'generic/platform=tvOS Simulator'")
  expect(source).toContain("build-target) cmd_build_target 'generic/platform=tvOS'")
  const gate = fs.readFileSync(path.join(root, 'scripts/ci/check-tvos-packed-consumer.sh'), 'utf8')
  expect(gate).toContain('for step in stage install verify-identity prebuild build-simulator build-target; do')
  expect(gate).not.toMatch(/build-simulator.*&|build-target.*&/u)
})
