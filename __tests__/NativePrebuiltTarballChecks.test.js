// __tests__/NativePrebuiltTarballChecks.test.js
//
// D2(iv) + PR210-18: the npm artifact ships CI-built native prebuilts for
// both mobile hosts. This suite unit-tests the tarball assertions (fixture
// file maps, no pack needed) — pass plus every negative.
//
// Changed expectations (PR210-18): the identity records are JSON
// (build-identity.json, replacing the line-oriented build-identity.txt);
// the Apple check now reads slices at <LibraryIdentifier>/<LibraryPath> —
// the real XCFramework layout; the old fixture placed archives at the
// framework root, which a real xcodebuild staging never does — and it
// verifies the exact declared slice set and every sha256 instead of
// counting LibraryIdentifier strings.

'use strict'

const crypto = require('crypto')

const { assertPackedAndroidPrebuilts, assertPackedRustCore } = require('../scripts/ci/verify-package-tarballs')
const {
  ANDROID_PREBUILT_SCHEMA,
  APPLE_DECLARED_LIBRARIES,
  APPLE_STAGING_SCHEMA
} = require('../scripts/release/native-build-identity')

const ANDROID_IDENTITY = 'package/android/src/main/jniLibs/build-identity.json'
const APPLE_IDENTITY = 'package/ios/RustCore/build-identity.json'
const FRAMEWORK = 'package/ios/RustCore/RustCore.xcframework/'
const LIB = 'libubm5_uniffi_echo.a'

function sha256(buffer) {
  return crypto.createHash('sha256').update(buffer).digest('hex')
}

function androidFixture() {
  const arm = Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x01, 0x02, 0x03, 0x04])
  const x64 = Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x05, 0x06, 0x07, 0x08, 0x09])
  const record = {
    schema: ANDROID_PREBUILT_SCHEMA,
    binding: 'jni',
    profile: 'release',
    abis: [
      { abi: 'arm64-v8a', target: 'aarch64-linux-android', file: 'libubm5_jni_echo.so', sha256: sha256(arm), bytes: 8 },
      { abi: 'x86_64', target: 'x86_64-linux-android', file: 'libubm5_jni_echo.so', sha256: sha256(x64), bytes: 9 }
    ]
  }
  return new Map([
    [ANDROID_IDENTITY, Buffer.from(JSON.stringify(record))],
    ['package/android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so', arm],
    ['package/android/src/main/jniLibs/x86_64/libubm5_jni_echo.so', x64]
  ])
}

function editRecord(files, identityPath, edit) {
  const record = JSON.parse(files.get(identityPath).toString('utf8'))
  edit(record)
  files.set(identityPath, Buffer.from(JSON.stringify(record)))
}

function appleFixture(libraries = APPLE_DECLARED_LIBRARIES) {
  const plist = Buffer.from('<?xml version="1.0"?>\n<plist><dict><key>AvailableLibraries</key></dict></plist>\n')
  const files = new Map([[`${FRAMEWORK}Info.plist`, plist]])
  const recorded = libraries.map(library => {
    const content = Buffer.from(`!<arch>\n${library.libraryIdentifier}\n`)
    files.set(`${FRAMEWORK}${library.libraryIdentifier}/${LIB}`, content)
    return { ...library, libraryPath: LIB, sha256: sha256(content), bytes: content.length }
  })
  files.set(
    APPLE_IDENTITY,
    Buffer.from(
      JSON.stringify({
        schema: APPLE_STAGING_SCHEMA,
        binding: 'uniffi',
        infoPlistSha256: sha256(plist),
        libraries: recorded
      })
    )
  )
  return files
}

describe('packed Android prebuilt checks (D2 iv)', () => {
  test('coherent prebuilt tree passes and reports bytes', () => {
    expect(assertPackedAndroidPrebuilts(androidFixture())).toBe(8 + 9)
  })

  test('missing identity fails', () => {
    const files = androidFixture()
    files.delete(ANDROID_IDENTITY)
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/missing Android prebuilt identity/)
  })

  test('a non-JSON or wrong-schema identity fails', () => {
    const files = androidFixture()
    files.set(ANDROID_IDENTITY, Buffer.from('abi=arm64-v8a sha256=00 bytes=8 file=libubm5_jni_echo.so\n'))
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/not valid JSON/)
    editRecord(files.set(ANDROID_IDENTITY, Buffer.from('{}')), ANDROID_IDENTITY, record => {
      record.schema = 'other'
    })
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/not a ubm-android-jnilibs-identity\/1 record/)
  })

  test('missing ABI entry fails', () => {
    const files = androidFixture()
    editRecord(files, ANDROID_IDENTITY, record => {
      record.abis = record.abis.filter(entry => entry.abi !== 'x86_64')
    })
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/no entry for ABI x86_64/)
  })

  test('an undeclared ABI fails', () => {
    const files = androidFixture()
    editRecord(files, ANDROID_IDENTITY, record => {
      record.abis.push({ ...record.abis[0], abi: 'armeabi-v7a' })
    })
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/undeclared ABI\(s\): armeabi-v7a/)
  })

  test('missing .so fails', () => {
    const files = androidFixture()
    files.delete('package/android/src/main/jniLibs/x86_64/libubm5_jni_echo.so')
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/missing Android prebuilt/)
  })

  test('byte mismatch fails', () => {
    const files = androidFixture()
    files.set('package/android/src/main/jniLibs/x86_64/libubm5_jni_echo.so', Buffer.from([0x00]))
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/is 1 bytes, identity says 9/)
  })

  test('sha mismatch fails', () => {
    const files = androidFixture()
    const tampered = Buffer.from(files.get('package/android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so'))
    tampered[7] = tampered[7] ^ 0xff
    files.set('package/android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so', tampered)
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/does not match identity/)
  })
})

describe('packed RustCore checks (D2 iv)', () => {
  test('coherent declared staging passes and reports bytes', () => {
    const files = appleFixture()
    const expected = [...files.entries()]
      .filter(([entry]) => entry.startsWith('package/ios/RustCore/'))
      .reduce((total, [, contents]) => total + contents.length, 0)
    expect(assertPackedRustCore(files)).toBe(expected)
  })

  test('unstaged tree passes with zero bytes (dev pack; release requires staging)', () => {
    expect(assertPackedRustCore(new Map())).toBe(0)
  })

  test('RustCore files without the identity record fail', () => {
    const files = appleFixture()
    files.delete(APPLE_IDENTITY)
    expect(() => assertPackedRustCore(files)).toThrow(/without build-identity\.json/)
  })

  test('staged identity without plist fails', () => {
    const files = appleFixture()
    files.delete(`${FRAMEWORK}Info.plist`)
    expect(() => assertPackedRustCore(files)).toThrow(/identity present but framework plist missing/)
  })

  test('an edited Info.plist fails the hash chain', () => {
    const files = appleFixture()
    files.set(`${FRAMEWORK}Info.plist`, Buffer.from('<plist/>'))
    expect(() => assertPackedRustCore(files)).toThrow(/Info\.plist sha256/)
  })

  test('an omitted slice fails as a declared-set mismatch', () => {
    const files = appleFixture(APPLE_DECLARED_LIBRARIES.slice(0, 2))
    expect(() => assertPackedRustCore(files)).toThrow(/differ from the declared set/)
  })

  test('missing declared slice fails', () => {
    const files = appleFixture()
    files.delete(`${FRAMEWORK}tvos-arm64/${LIB}`)
    expect(() => assertPackedRustCore(files)).toThrow(/slice is missing/)
  })

  test('empty slice fails', () => {
    const files = appleFixture()
    files.set(`${FRAMEWORK}ios-arm64/${LIB}`, Buffer.alloc(0))
    expect(() => assertPackedRustCore(files)).toThrow(/slice is empty/)
  })

  test('a substituted slice fails', () => {
    const files = appleFixture()
    files.set(`${FRAMEWORK}ios-arm64/${LIB}`, Buffer.from('other object code'))
    expect(() => assertPackedRustCore(files)).toThrow(/does not match build-identity\.json/)
  })

  test('an undeclared archive fails', () => {
    const files = appleFixture()
    files.set(`${FRAMEWORK}ios-arm64/stale.a`, Buffer.from('x'))
    expect(() => assertPackedRustCore(files)).toThrow(/undeclared archive/)
  })

  test('escaping LibraryPath fails', () => {
    const files = appleFixture()
    editRecord(files, APPLE_IDENTITY, record => {
      record.libraries[0].libraryPath = '../../evil.a'
    })
    expect(() => assertPackedRustCore(files)).toThrow(/escapes the framework/)
  })
})
