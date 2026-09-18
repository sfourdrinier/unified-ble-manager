// __tests__/IosVerifyRustCore.test.js
//
// PR210-18: ios/verify-rust-core.sh replaces the `grep -c LibraryIdentifier`
// slice count with a parsed (plutil) check of the exact declared slice set
// plus the Info.plist and per-slice hash chain. This suite runs the real
// script against a synthetic XCFramework (XML Info.plist, stub archives),
// so it exercises the verifier, not an Apple build. plutil exists only on
// macOS; on other hosts each executed case instead asserts that the macOS
// producer job runs the verifier (the route that executes it there).

'use strict'

const crypto = require('crypto')
const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const { APPLE_DECLARED_LIBRARIES, APPLE_STAGING_SCHEMA } = require('../scripts/release/native-build-identity')

const root = path.join(__dirname, '..')
const verifier = path.join(root, 'ios', 'verify-rust-core.sh')
const LIB = 'libubm5_uniffi_echo.a'

function sha256(content) {
  return crypto.createHash('sha256').update(content).digest('hex')
}

function plistXml(libraries) {
  const entries = libraries
    .map(library => {
      const archs = library.architectures.map(arch => `<string>${arch}</string>`).join('')
      const variant =
        library.variant === '' ? '' : `<key>SupportedPlatformVariant</key><string>${library.variant}</string>`
      return `<dict><key>LibraryIdentifier</key><string>${library.libraryIdentifier}</string><key>LibraryPath</key><string>${LIB}</string><key>SupportedArchitectures</key><array>${archs}</array><key>SupportedPlatform</key><string>${library.platform}</string>${variant}</dict>`
    })
    .join('')
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict><key>AvailableLibraries</key><array>${entries}</array><key>CFBundlePackageType</key><string>XFWK</string></dict></plist>\n`
}

function stage(dir, libraries = APPLE_DECLARED_LIBRARIES) {
  const framework = path.join(dir, 'RustCore.xcframework')
  fs.mkdirSync(framework, { recursive: true })
  const plist = path.join(framework, 'Info.plist')
  fs.writeFileSync(plist, plistXml(libraries))
  const recorded = libraries.map(library => {
    const content = Buffer.from(`!<arch>\nslice ${library.libraryIdentifier}\n`)
    fs.mkdirSync(path.join(framework, library.libraryIdentifier), { recursive: true })
    fs.writeFileSync(path.join(framework, library.libraryIdentifier, LIB), content)
    return { ...library, libraryPath: LIB, sha256: sha256(content), bytes: content.length }
  })
  fs.writeFileSync(
    path.join(dir, 'build-identity.json'),
    JSON.stringify({
      schema: APPLE_STAGING_SCHEMA,
      binding: 'uniffi',
      infoPlistSha256: sha256(fs.readFileSync(plist)),
      libraries: recorded
    })
  )
  return framework
}

function verify(dir) {
  return spawnSync('sh', [verifier, '--dir', dir], { encoding: 'utf8' })
}

test('verify-rust-core.sh is executable and syntax-clean', () => {
  fs.accessSync(verifier, fs.constants.X_OK)
  expect(spawnSync('sh', ['-n', verifier]).status).toBe(0)
})

function executesHere() {
  if (process.platform === 'darwin') return true
  const publish = fs.readFileSync(path.join(root, '.github', 'workflows', 'publish.yml'), 'utf8')
  expect(publish).toContain('sh ios/verify-rust-core.sh')
  return false
}

describe('verify-rust-core.sh against a synthetic staging (macOS plutil)', () => {
  let dir
  beforeEach(() => {
    dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-verify-rustcore-'))
  })
  afterEach(() => {
    fs.rmSync(dir, { recursive: true, force: true })
  })

  test('the declared four slices with matching hashes pass', () => {
    if (!executesHere()) return
    stage(dir)
    const result = verify(dir)
    expect(result.stderr).toBe('')
    expect(result.status).toBe(0)
    expect(result.stdout).toContain('OK 4 slices')
  })

  test('an omitted simulator slice fails (the old grep counted lines, not slices)', () => {
    if (!executesHere()) return
    stage(
      dir,
      APPLE_DECLARED_LIBRARIES.filter(library => library.libraryIdentifier !== 'tvos-arm64-simulator')
    )
    const result = verify(dir)
    expect(result.status).not.toBe(0)
    expect(result.stderr).toContain('differ from the declared set')
  })

  test('a slice with the right identifier but the wrong architecture set fails', () => {
    if (!executesHere()) return
    stage(
      dir,
      APPLE_DECLARED_LIBRARIES.map(library =>
        library.libraryIdentifier === 'ios-arm64_x86_64-simulator' ? { ...library, architectures: ['arm64'] } : library
      )
    )
    expect(verify(dir).stderr).toContain('differ from the declared set')
  })

  test('a substituted slice fails and names it', () => {
    if (!executesHere()) return
    const framework = stage(dir)
    fs.appendFileSync(path.join(framework, 'ios-arm64', LIB), 'tampered')
    const result = verify(dir)
    expect(result.status).not.toBe(0)
    expect(result.stderr).toContain('slice ios-arm64/libubm5_uniffi_echo.a sha256 does not match')
  })

  test('an edited Info.plist fails the hash chain', () => {
    if (!executesHere()) return
    const framework = stage(dir)
    const plist = path.join(framework, 'Info.plist')
    fs.writeFileSync(plist, fs.readFileSync(plist, 'utf8').replace('XFWK', 'XFWX'))
    expect(verify(dir).stderr).toContain('Info.plist sha256 does not match')
  })

  test('an undeclared extra archive fails', () => {
    if (!executesHere()) return
    const framework = stage(dir)
    fs.writeFileSync(path.join(framework, 'tvos-arm64', 'stale.a'), 'x')
    expect(verify(dir).stderr).toContain('undeclared archive tvos-arm64/stale.a')
  })

  test('a missing identity record fails with the prepare command', () => {
    if (!executesHere()) return
    stage(dir)
    fs.rmSync(path.join(dir, 'build-identity.json'))
    const result = verify(dir)
    expect(result.status).not.toBe(0)
    expect(result.stderr).toContain('native:apple:prepare')
  })
})
