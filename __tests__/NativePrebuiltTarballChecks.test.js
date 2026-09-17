// __tests__/NativePrebuiltTarballChecks.test.js
//
// D2(iv): the npm artifact ships CI-built native prebuilts for both mobile
// hosts. This suite unit-tests the tarball assertions (fixture file maps,
// no pack needed) — pass plus every negative: missing identity, missing
// slice, byte mismatch, sha mismatch, wrong slice count, escaping
// LibraryPath.

const crypto = require('crypto')

const {
  assertPackedAndroidPrebuilts,
  assertPackedRustCore
} = require('../scripts/ci/verify-package-tarballs')

function sha256(buffer) {
  return crypto.createHash('sha256').update(buffer).digest('hex')
}

function androidFixture() {
  const arm = Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x01, 0x02, 0x03, 0x04])
  const x64 = Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x05, 0x06, 0x07, 0x08, 0x09])
  const identity =
    'profile=release\nabis=arm64-v8a,x86_64\n' +
    `abi=arm64-v8a sha256=${sha256(arm)} bytes=${arm.length} file=libubm5_jni_echo.so\n` +
    `abi=x86_64 sha256=${sha256(x64)} bytes=${x64.length} file=libubm5_jni_echo.so\n`
  return new Map([
    ['package/android/src/main/jniLibs/build-identity.txt', Buffer.from(identity)],
    ['package/android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so', arm],
    ['package/android/src/main/jniLibs/x86_64/libubm5_jni_echo.so', x64]
  ])
}

const APPLE_IDENTIFIERS = ['ios-arm64', 'ios-arm64-simulator', 'tvos-arm64', 'tvos-arm64-simulator']

function applePlist(identifiers) {
  const libraries = identifiers
    .map(
      identifier =>
        `<dict><key>LibraryIdentifier</key><string>${identifier}</string>` +
        `<key>LibraryPath</key><string>libubm5_uniffi_echo_${identifier}.a</string></dict>`
    )
    .join('')
  return Buffer.from(`<?xml version="1.0"?>\n<plist><dict><key>AvailableLibraries</key><array>${libraries}</array></dict></plist>\n`)
}

function appleFixture(identifiers = APPLE_IDENTIFIERS) {
  const files = new Map([
    ['package/ios/RustCore/build-identity.txt', Buffer.from('profile=release\nslices=4\n')],
    ['package/ios/RustCore/RustCore.xcframework/Info.plist', applePlist(identifiers)]
  ])
  for (const identifier of identifiers) {
    files.set(
      `package/ios/RustCore/RustCore.xcframework/libubm5_uniffi_echo_${identifier}.a`,
      Buffer.from([0x21, 0x3c, 0x61, 0x72, 0x63, 0x68, 0x3e, 0x0a])
    )
  }
  return files
}

describe('packed Android prebuilt checks (D2 iv)', () => {
  test('coherent prebuilt tree passes and reports bytes', () => {
    expect(assertPackedAndroidPrebuilts(androidFixture())).toBe(8 + 9)
  })

  test('missing identity fails', () => {
    const files = androidFixture()
    files.delete('package/android/src/main/jniLibs/build-identity.txt')
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/missing Android prebuilt identity/)
  })

  test('missing ABI entry fails', () => {
    const files = androidFixture()
    const identity = files.get('package/android/src/main/jniLibs/build-identity.txt').toString('utf8')
    files.set(
      'package/android/src/main/jniLibs/build-identity.txt',
      Buffer.from(identity.split('\n').filter(line => !line.startsWith('abi=x86_64 ')).join('\n'))
    )
    expect(() => assertPackedAndroidPrebuilts(files)).toThrow(/no entry for ABI x86_64/)
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
  test('coherent 4-slice staging passes and reports bytes', () => {
    const files = appleFixture()
    const expected =
      files.get('package/ios/RustCore/RustCore.xcframework/Info.plist').length +
      files.get('package/ios/RustCore/build-identity.txt').length +
      4 * 8
    expect(assertPackedRustCore(files)).toBe(expected)
  })

  test('unstaged tree passes with zero bytes (dev pack; release requires staging)', () => {
    const files = appleFixture()
    files.delete('package/ios/RustCore/build-identity.txt')
    files.delete('package/ios/RustCore/RustCore.xcframework/Info.plist')
    expect(assertPackedRustCore(files)).toBe(0)
  })

  test('staged identity without plist fails', () => {
    const files = appleFixture()
    files.delete('package/ios/RustCore/RustCore.xcframework/Info.plist')
    expect(() => assertPackedRustCore(files)).toThrow(/identity present but framework plist missing/)
  })

  test('wrong slice count fails', () => {
    const files = appleFixture(['ios-arm64', 'ios-arm64-simulator'])
    expect(() => assertPackedRustCore(files)).toThrow(/exactly 4 platform slices, found 2/)
  })

  test('missing declared slice fails', () => {
    const files = appleFixture()
    files.delete('package/ios/RustCore/RustCore.xcframework/libubm5_uniffi_echo_tvos-arm64.a')
    expect(() => assertPackedRustCore(files)).toThrow(/slice is missing/)
  })

  test('empty slice fails', () => {
    const files = appleFixture()
    files.set('package/ios/RustCore/RustCore.xcframework/libubm5_uniffi_echo_ios-arm64.a', Buffer.alloc(0))
    expect(() => assertPackedRustCore(files)).toThrow(/slice is empty/)
  })

  test('escaping LibraryPath fails', () => {
    const files = appleFixture()
    const evil = applePlist(APPLE_IDENTIFIERS).toString('utf8').replace(
      'libubm5_uniffi_echo_ios-arm64.a',
      '../../evil.a'
    )
    files.set('package/ios/RustCore/RustCore.xcframework/Info.plist', Buffer.from(evil))
    expect(() => assertPackedRustCore(files)).toThrow(/escapes the framework/)
  })
})
