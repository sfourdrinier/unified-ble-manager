'use strict'

// F9: `pnpm native:status` classifies every precompiled Rust artifact as
// fresh, stale, missing or not-applicable, reusing
// scripts/release/native-build-identity.js (no second digest
// implementation). Hermetic: temporary fixture roots plus a fake identity
// module, so no real artifact is ever built or read here.

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

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

function androidIdentity(staged) {
  return {
    schema: 'ubm-android-jnilibs-identity/1',
    binding: 'jni',
    contractRevision: 'rev',
    sourceDigest: staged.sourceDigest,
    bindingSchema: staged.bindingSchema,
    profile: 'release',
    toolchain: 'test',
    ndk: 'test',
    abis: []
  }
}

function appleIdentity(staged) {
  return {
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
  }
}

function writeDesktopPrebuild(root, dir, staged) {
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
  return addon
}

function tempRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-native-status-'))
}

describe('native:status classification', () => {
  test('fresh android reports staged and current digests with its refresh command', () => {
    const root = tempRoot()
    const current = fixtureCurrent()
    writeJson(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'), androidIdentity(current.jni))
    const result = status.classifyAndroid(root, fakeIdentity(current))
    expect(result.state).toBe('fresh')
    expect(result.staged).toEqual(current.jni)
    expect(result.current).toEqual(current.jni)
    const line = status.formatResultLine(result)
    expect(line).toContain('android: fresh')
    expect(line).toContain(DIGEST_A)
    expect(line).toContain('android/refresh-prebuilt-jniLibs.sh')
    expect(line.split('\n')).toHaveLength(1)
  })

  test('stale android names the differing digests', () => {
    const root = tempRoot()
    writeJson(
      path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'),
      androidIdentity({ sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    )
    const result = status.classifyAndroid(root, fakeIdentity(fixtureCurrent()))
    expect(result.state).toBe('stale')
    expect(status.formatResultLine(result)).toMatch(/android: stale.*staged=.*current=/)
  })

  test('missing android identity is missing, not stale', () => {
    const result = status.classifyAndroid(tempRoot(), fakeIdentity(fixtureCurrent()))
    expect(result.state).toBe('missing')
    expect(status.formatResultLine(result)).toContain('android: missing')
  })

  test('a broken hash chain is stale even when the digests match', () => {
    const root = tempRoot()
    const current = fixtureCurrent()
    writeJson(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'), androidIdentity(current.jni))
    const failing = fakeIdentity(current, { android: 'some slice went missing' })
    expect(status.classifyAndroid(root, failing).state).toBe('stale')
  })

  test('apple is fresh on darwin and not-applicable elsewhere, never silently skipped', () => {
    const root = tempRoot()
    const current = fixtureCurrent()
    writeJson(path.join(root, 'ios', 'RustCore', 'build-identity.json'), appleIdentity(current.uniffi))
    expect(status.classifyApple(root, fakeIdentity(current), 'darwin').state).toBe('fresh')
    const other = status.classifyApple(root, fakeIdentity(current), 'linux')
    expect(other.state).toBe('not-applicable')
    expect(status.formatResultLine(other)).toMatch(/apple: not-applicable/)
  })

  test('stale and missing apple on darwin', () => {
    const root = tempRoot()
    expect(status.classifyApple(root, fakeIdentity(fixtureCurrent()), 'darwin').state).toBe('missing')
    writeJson(
      path.join(root, 'ios', 'RustCore', 'build-identity.json'),
      appleIdentity({ sourceDigest: DIGEST_C, bindingSchema: DIGEST_C })
    )
    expect(status.classifyApple(root, fakeIdentity(fixtureCurrent()), 'darwin').state).toBe('stale')
  })

  test('desktop classifies the host prebuild and lists every other platform as not-applicable', () => {
    const root = tempRoot()
    const current = fixtureCurrent()
    writeDesktopPrebuild(root, 'darwin-arm64', current.napi)
    const results = status.classifyDesktop(root, fakeIdentity(current), 'darwin', 'arm64')
    const host = results.find(entry => entry.name === 'desktop')
    expect(host.state).toBe('fresh')
    expect(status.formatResultLine(host)).toContain('build-napi-addon.js')
    const others = results.filter(entry => entry.name !== 'desktop')
    expect(others.length).toBeGreaterThan(0)
    for (const entry of others) {
      expect(entry.state).toBe('not-applicable')
      expect(status.formatResultLine(entry)).toContain('not-applicable')
    }
    expect(others.some(entry => entry.name.includes('win32'))).toBe(true)
  })

  test('desktop with a substituted binary or an older build is stale; absent is missing', () => {
    const root = tempRoot()
    expect(status.classifyDesktop(root, fakeIdentity(fixtureCurrent()), 'darwin', 'arm64')[0].state).toBe('missing')
    writeDesktopPrebuild(root, 'darwin-arm64', { sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    const stale = status.classifyDesktop(root, fakeIdentity(fixtureCurrent()), 'darwin', 'arm64')[0]
    expect(stale.state).toBe('stale')
    const addon = path.join(root, 'native', 'desktop-core', 'prebuilds', 'darwin-arm64', 'ubm_desktop_core.node')
    fs.writeFileSync(addon, 'substituted')
    expect(status.classifyDesktop(root, fakeIdentity(fixtureCurrent()), 'darwin', 'arm64')[0].state).toBe('stale')
  })
})

describe('native:status exit code', () => {
  function fullRoot(current) {
    const root = tempRoot()
    writeJson(path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'), androidIdentity(current.jni))
    writeJson(path.join(root, 'ios', 'RustCore', 'build-identity.json'), appleIdentity(current.uniffi))
    writeDesktopPrebuild(root, 'darwin-arm64', current.napi)
    return root
  }

  test('exit 0 when everything applicable is fresh; non-zero on stale or missing', () => {
    const current = fixtureCurrent()
    const fresh = status.runStatus({
      root: fullRoot(current),
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64'
    })
    expect(fresh.exitCode).toBe(0)
    const staleRoot = fullRoot(current)
    writeJson(
      path.join(staleRoot, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'),
      androidIdentity({ sourceDigest: DIGEST_C, bindingSchema: DIGEST_B })
    )
    expect(
      status.runStatus({ root: staleRoot, identity: fakeIdentity(current), platform: 'darwin', arch: 'arm64' }).exitCode
    ).not.toBe(0)
    expect(
      status.runStatus({ root: tempRoot(), identity: fakeIdentity(current), platform: 'darwin', arch: 'arm64' })
        .exitCode
    ).not.toBe(0)
  })

  test('not-applicable artifacts never fail the check', () => {
    const current = fixtureCurrent()
    const root = fullRoot(current)
    writeDesktopPrebuild(root, 'linux-x64', current.napi)
    const onLinux = status.runStatus({ root, identity: fakeIdentity(current), platform: 'linux', arch: 'x64' })
    expect(onLinux.results.some(entry => entry.state === 'not-applicable')).toBe(true)
    expect(onLinux.exitCode).toBe(0)
  })

  test('--only filters the gate and --json stays single-parseable', () => {
    const current = fixtureCurrent()
    const root = fullRoot(current)
    const filtered = status.runStatus({
      root,
      identity: fakeIdentity(current),
      platform: 'darwin',
      arch: 'arm64',
      only: ['android']
    })
    expect(filtered.exitCode).toBe(0)
    expect(filtered.results.map(entry => entry.group)).toEqual(['android'])
    const parsed = JSON.parse(filtered.json)
    expect(parsed.map(entry => entry.name)).toEqual(['android'])
    expect(String(filtered.json).split('\n').filter(Boolean)).toHaveLength(1)
  })
})
