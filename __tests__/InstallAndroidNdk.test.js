// __tests__/InstallAndroidNdk.test.js

'use strict'

// A single truncated NDK download ("Error reading Zip content from a
// SeekableByteChannel") failed the 5.1.3 Expo SDK 57 release gate: sdkmanager
// unzips whatever arrived, exits 1, and nothing retried. The installer retries
// from a clean slate and accepts only the revision android/build.gradle pins.

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { installNdk, installedNdkRevision, pinnedNdkVersion } = require('../scripts/ci/install-android-ndk')

const VERSION = '27.1.12297006'

function sdkRoot() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-android-sdk-'))
}

function writeNdk(root, revision) {
  const ndkDir = path.join(root, 'ndk', VERSION)
  fs.mkdirSync(ndkDir, { recursive: true })
  fs.writeFileSync(path.join(ndkDir, 'source.properties'), `Pkg.Desc = Android NDK\nPkg.Revision = ${revision}\n`)
}

function writePartialDownload(root) {
  const ndkDir = path.join(root, 'ndk', VERSION)
  fs.mkdirSync(ndkDir, { recursive: true })
  fs.writeFileSync(path.join(ndkDir, 'truncated.bin'), 'partial')
  fs.mkdirSync(path.join(root, '.temp'), { recursive: true })
  fs.writeFileSync(path.join(root, '.temp', 'android-ndk-r27b-linux.zip'), 'partial')
}

function harness(root, outcomes) {
  const installs = []
  const sleeps = []
  const logs = []
  return {
    installs,
    sleeps,
    logs,
    options: {
      sdkRoot: root,
      version: VERSION,
      attempts: 3,
      delayMs: 10,
      install: pkg => {
        installs.push(pkg)
        return outcomes[installs.length - 1]()
      },
      sleep: ms => sleeps.push(ms),
      log: line => logs.push(line)
    }
  }
}

describe('install-android-ndk', () => {
  test('pins the NDK android/build.gradle declares as its default', () => {
    const gradle = fs.readFileSync(path.join(__dirname, '..', 'android', 'build.gradle'), 'utf8')
    expect(gradle).toContain(`: "${pinnedNdkVersion()}"`)
    expect(
      pinnedNdkVersion('ndkVersion = rootProject.ext.has("ndkVersion") ? rootProject.ext.ndkVersion : "1.2.3"')
    ).toBe('1.2.3')
    expect(() => pinnedNdkVersion('android {}')).toThrow('declares no default ndkVersion')
  })

  test('retries a truncated download from a clean slate and reports the attempt that succeeded', () => {
    const root = sdkRoot()
    const run = harness(root, [
      () => {
        writePartialDownload(root)
        return 1
      },
      () => {
        expect(fs.existsSync(path.join(root, 'ndk', VERSION, 'truncated.bin'))).toBe(false)
        expect(fs.existsSync(path.join(root, '.temp'))).toBe(false)
        writeNdk(root, VERSION)
        return 0
      }
    ])

    expect(installNdk(run.options)).toBe(2)
    expect(run.installs).toEqual([`ndk;${VERSION}`, `ndk;${VERSION}`])
    expect(run.sleeps).toEqual([10])
    expect(run.logs[0]).toContain(`::warning::NDK ${VERSION} install attempt 1 of 3 failed (sdkmanager exited 1)`)
    expect(installedNdkRevision(path.join(root, 'ndk', VERSION))).toBe(VERSION)
  })

  test('treats an exit 0 that left the wrong or no revision as a failed attempt', () => {
    const root = sdkRoot()
    const run = harness(root, [
      () => 0,
      () => {
        writeNdk(root, '27.0.12077973')
        return 0
      },
      () => {
        writeNdk(root, VERSION)
        return 0
      }
    ])

    expect(installNdk(run.options)).toBe(3)
    expect(run.logs[0]).toContain('installed revision is missing')
    expect(run.logs[1]).toContain('installed revision is 27.0.12077973')
    expect(run.sleeps).toEqual([10, 20])
  })

  test('fails with the last reason once the bounded attempts are spent', () => {
    const root = sdkRoot()
    const run = harness(root, [() => 1, () => 1, () => 137])

    expect(() => installNdk(run.options)).toThrow(
      `NDK ${VERSION} did not install after 3 attempts (last: sdkmanager exited 137)`
    )
    expect(run.installs).toHaveLength(3)
    expect(run.sleeps).toEqual([10, 20])
  })

  test('skips sdkmanager when the pinned revision is already installed', () => {
    const root = sdkRoot()
    writeNdk(root, VERSION)
    const run = harness(root, [])

    expect(installNdk(run.options)).toBe(0)
    expect(run.installs).toEqual([])
    expect(run.logs).toEqual([`NDK ${VERSION} is already installed at ${path.join(root, 'ndk', VERSION)}`])
  })

  test('reports an unreadable source.properties rather than treating it as absent', () => {
    const root = sdkRoot()
    fs.mkdirSync(path.join(root, 'ndk', VERSION, 'source.properties'), { recursive: true })

    expect(() => installedNdkRevision(path.join(root, 'ndk', VERSION))).toThrow()
  })
})
