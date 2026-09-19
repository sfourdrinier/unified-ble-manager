'use strict'

// F9: `pnpm native:refresh --only <groups>` rebuilds only stale artifacts
// with the canonical builders, then re-checks and fails while anything is
// still stale. Hermetic: the builder commands are injected strings run
// through an injected runner, so no real toolchain ever runs here.

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const refresh = require('../scripts/native/native-refresh')
const status = require('../scripts/native/native-status')

const DIGEST_A = 'a'.repeat(64)
const DIGEST_B = 'b'.repeat(64)
const DIGEST_C = 'c'.repeat(64)

function fakeIdentity(current, failures = {}) {
  return {
    computeBindingIdentity: (root, binding) => ({ ...current[binding] }),
    checkAndroidPrebuilts: root => {
      if (failures.android !== undefined) throw new Error(failures.android)
    },
    checkAppleStaging: (root, dir) => {
      if (failures.apple !== undefined) throw new Error(failures.apple)
    }
  }
}

function fixtureCurrent() {
  return {
    jni: { sourceDigest: DIGEST_A, bindingSchema: DIGEST_B },
    uniffi: { sourceDigest: DIGEST_A, bindingSchema: DIGEST_B },
    napi: { sourceDigest: DIGEST_A, bindingSchema: DIGEST_B }
  }
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`)
}

function writeAndroid(root, staged) {
  writeJson(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'), {
    schema: 'ubm-android-jnilibs-identity/1',
    binding: 'jni',
    contractRevision: 'rev',
    sourceDigest: staged.sourceDigest,
    bindingSchema: staged.bindingSchema,
    profile: 'release',
    toolchain: 'test',
    ndk: 'test',
    abis: []
  })
}

function writeApple(root, staged) {
  writeJson(path.join(root, 'ios', 'RustCore', 'build-identity.json'), {
    schema: 'ubm-apple-rustcore-identity/1',
    binding: 'uniffi',
    contractRevision: 'rev',
    sourceDigest: staged.sourceDigest,
    bindingSchema: staged.bindingSchema,
    profile: 'release',
    toolchain: 'test',
    xcodebuild: 'test',
    infoPlistSha256: '0'.repeat(64),
    libraries: []
  })
}

function writeDesktop(root, dir, staged) {
  const addon = path.join(root, 'native', 'desktop-core', 'prebuilds', dir, 'ubm_desktop_core.node')
  fs.mkdirSync(path.dirname(addon), { recursive: true })
  fs.writeFileSync(addon, 'fake-binary')
  const sha256 = require('node:crypto').createHash('sha256').update(fs.readFileSync(addon)).digest('hex')
  writeJson(path.join(path.dirname(addon), 'ubm_desktop_core.identity.json'), {
    schema: 'ubm-desktop-core-prebuild/1',
    sha256,
    bytes: fs.statSync(addon).size,
    identity: JSON.stringify({ sourceDigest: staged.sourceDigest, bindingSchema: staged.bindingSchema })
  })
}

function freshRoot(current) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-native-refresh-'))
  writeAndroid(root, current.jni)
  writeApple(root, current.uniffi)
  writeDesktop(root, 'darwin-arm64', current.napi)
  return root
}

const BUILDERS = Object.freeze({
  android: 'rebuild-android',
  apple: 'rebuild-apple',
  desktop: 'rebuild-desktop'
})

describe('native:refresh', () => {
  test('fresh artifacts are a no-op: no builder runs', () => {
    const current = fixtureCurrent()
    const outcome = refresh.runRefresh({
      groups: ['android', 'apple', 'desktop'],
      root: freshRoot(current),
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64',
      builders: BUILDERS,
      runCommand: command => {
        throw new Error(`no builder should run, got: ${command}`)
      }
    })
    expect(outcome.exitCode).toBe(0)
    expect(outcome.output).toEqual([])
  })

  test('only the stale builders run, sequentially', () => {
    const current = fixtureCurrent()
    const root = freshRoot(current)
    writeAndroid(root, { sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    fs.rmSync(path.join(root, 'native', 'desktop-core', 'prebuilds', 'darwin-arm64'), { recursive: true, force: true })
    const ran = []
    const outcome = refresh.runRefresh({
      groups: ['android', 'apple', 'desktop'],
      root,
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64',
      builders: BUILDERS,
      runCommand: command => {
        ran.push(command)
        if (command === 'rebuild-android') writeAndroid(root, current.jni)
        if (command === 'rebuild-desktop') writeDesktop(root, 'darwin-arm64', current.napi)
      }
    })
    expect(ran).toEqual(['rebuild-android', 'rebuild-desktop'])
    expect(outcome.exitCode).toBe(0)
    expect(outcome.output).toHaveLength(2)
    const after = status.runStatus({ root, identity: fakeIdentity(current), platform: 'darwin', arch: 'arm64' })
    expect(after.exitCode).toBe(0)
  })

  test('a rebuilt line names what was rebuilt and why (old and new digest)', () => {
    const current = fixtureCurrent()
    const root = freshRoot(current)
    writeApple(root, { sourceDigest: DIGEST_C, bindingSchema: DIGEST_C })
    const outcome = refresh.runRefresh({
      groups: ['apple'],
      root,
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64',
      builders: BUILDERS,
      runCommand: command => {
        expect(command).toBe('rebuild-apple')
        writeApple(root, current.uniffi)
      }
    })
    expect(outcome.exitCode).toBe(0)
    expect(outcome.output).toHaveLength(1)
    expect(outcome.output[0].split('\n')).toHaveLength(1)
    expect(outcome.output[0]).toContain('apple')
    expect(outcome.output[0]).toContain(DIGEST_C)
    expect(outcome.output[0]).toContain(DIGEST_A)
  })

  test('still stale after the build fails instead of continuing', () => {
    const current = fixtureCurrent()
    const root = freshRoot(current)
    writeAndroid(root, { sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    const outcome = refresh.runRefresh({
      groups: ['android'],
      root,
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64',
      builders: BUILDERS,
      runCommand: () => {}
    })
    expect(outcome.exitCode).not.toBe(0)
    expect(outcome.error).toMatch(/still stale/)
  })

  test('a builder failure aborts with the builder command and runs nothing after it', () => {
    const current = fixtureCurrent()
    const root = freshRoot(current)
    writeAndroid(root, { sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    writeApple(root, { sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    const ran = []
    const outcome = refresh.runRefresh({
      groups: ['android', 'apple'],
      root,
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64',
      builders: BUILDERS,
      runCommand: command => {
        ran.push(command)
        if (command === 'rebuild-android') throw new Error('simulated NDK failure')
      }
    })
    expect(outcome.exitCode).not.toBe(0)
    expect(ran).toEqual(['rebuild-android'])
    expect(outcome.error).toContain('rebuild-android')
  })
})
