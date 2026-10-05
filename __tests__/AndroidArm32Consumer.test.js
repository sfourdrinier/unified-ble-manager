const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')
const root = path.resolve(__dirname, '..')
const read = name => fs.readFileSync(path.join(root, name), 'utf8')

test('every maintained prebuilt can be committed while incidental JNI output stays ignored', () => {
  const { ANDROID_DECLARED_ABIS } = require('../scripts/release/native-build-identity')
  for (const { abi } of ANDROID_DECLARED_ABIS) {
    const shipped = spawnSync(
      'git',
      ['check-ignore', '--no-index', '--', `android/src/main/jniLibs/${abi}/libubm5_jni_echo.so`],
      { cwd: root, encoding: 'utf8' }
    )
    expect(shipped.error).toBeUndefined()
    expect(shipped.status).toBe(1)
    const incidental = spawnSync(
      'git',
      ['check-ignore', '--no-index', '--', `android/src/main/jniLibs/${abi}/incidental.so`],
      { cwd: root, encoding: 'utf8' }
    )
    expect(incidental.error).toBeUndefined()
    expect(incidental.status).toBe(0)
  }
})

test('maintained Android producers and shipping proof cover ARM32 without recompiling packed library sources', () => {
  const identity = require('../scripts/release/native-build-identity')
  expect(identity.ANDROID_DECLARED_ABIS).toContainEqual({ abi: 'armeabi-v7a', target: 'armv7-linux-androideabi' })
  const abis = identity.ANDROID_DECLARED_ABIS.map(({ abi }) => abi)
  for (const [file, variable] of [
    ['android/build-rust-cdylib.sh', 'DECLARED_ABIS'],
    ['android/refresh-prebuilt-jniLibs.sh', 'ABIS']
  ]) {
    expect(
      read(file)
        .match(new RegExp(`${variable}="([^"]+)"`))[1]
        .split(' ')
    ).toEqual(abis)
  }
  expect(
    read('android/build.gradle')
      .match(/def ubmRustAbis = \[([^\]]+)\]/u)[1]
      .match(/"[^"]+"/gu)
      .map(value => JSON.parse(value))
  ).toEqual(abis)
  for (const workflow of ['.github/workflows/ci.yml', '.github/workflows/publish.yml']) {
    expect(read(workflow)).toContain('armv7-linux-androideabi aarch64-linux-android x86_64-linux-android')
    expect(read(workflow)).toContain('-PreactNativeArchitectures=armeabi-v7a,arm64-v8a')
  }
  expect(read('.github/workflows/publish.yml')).toContain('check-android-tv-packed-consumer.sh')
  const androidFilter = read('.github/workflows/ci.yml').split('            android:')[1].split('            apple:')[0]
  expect(androidFilter).toContain("'scripts/ci/check-android*'")
  const proof = read('scripts/ci/check-android-tv-packed-consumer.sh')
  expect(proof).toContain('TV_PACKAGE_TARBALL')
  expect(proof).toContain('UBM_NATIVE_BUILD=prebuilt')
  expect(proof).toContain('prebuild-android build-android')
  expect(proof).toContain('check-android-apk-abi.js')
  expect(read('example-expo/scripts/build-tv.sh')).toContain('npm:react-native-tvos@0.86.3-0')
  expect(JSON.parse(read('example-expo/package.json')).dependencies.expo).toMatch(/^~57\./u)
  expect(read('docs/TV.md')).toContain("legacyLocation: 'auto'")
  expect(read('docs/TV.md')).toContain("manager.permissions.request({ purpose: 'scan-and-connect' })")
})

function sharedElf(needed = [], elfClass = 1, machine = 40) {
  const bytes = Buffer.alloc(1024)
  const is32 = elfClass === 1
  bytes.set([0x7f, 69, 76, 70, elfClass, 1, 1])
  bytes.writeUInt16LE(3, 16)
  bytes.writeUInt16LE(machine, 18)
  bytes.writeUInt32LE(1, 20)
  const header = is32 ? 52 : 64
  const entry = is32 ? 32 : 56
  const word = (offset, value) =>
    is32 ? bytes.writeUInt32LE(value, offset) : bytes.writeBigUInt64LE(BigInt(value), offset)
  word(is32 ? 28 : 32, header)
  bytes.writeUInt16LE(header, is32 ? 40 : 52)
  bytes.writeUInt16LE(entry, is32 ? 42 : 54)
  bytes.writeUInt16LE(2, is32 ? 44 : 56)
  const segment = (offset, type, fileOffset, size) => {
    bytes.writeUInt32LE(type, offset)
    if (!is32) bytes.writeUInt32LE(6, offset + 4)
    word(offset + (is32 ? 4 : 8), fileOffset)
    word(offset + (is32 ? 8 : 16), 0x10000 + fileOffset)
    word(offset + (is32 ? 16 : 32), size)
    word(offset + (is32 ? 20 : 40), size)
    if (is32) bytes.writeUInt32LE(6, offset + 24)
    word(offset + (is32 ? 28 : 48), type === 1 ? 4096 : is32 ? 4 : 8)
  }
  const dynamicEntry = is32 ? 8 : 16
  segment(header, 1, 0, bytes.length)
  segment(header + entry, 2, 256, (needed.length + 3) * dynamicEntry)
  const strings = Buffer.from('\0' + needed.join('\0') + '\0')
  strings.copy(bytes, 768)
  let index = 0
  const dynamic = (tag, value) => {
    word(256 + index * dynamicEntry, tag)
    word(256 + index++ * dynamicEntry + dynamicEntry / 2, value)
  }
  dynamic(5, 0x10000 + 768)
  dynamic(10, strings.length)
  let stringOffset = 1
  for (const name of needed) {
    dynamic(1, stringOffset)
    stringOffset += Buffer.byteLength(name) + 1
  }
  dynamic(0, 0)
  return bytes
}

test('APK ABI proof validates every ARM32 object and rejects incomplete native graphs', () => {
  const { validateAndroidApkAbi } = require('../scripts/ci/check-android-apk-abi')
  const elf = sharedElf(['libc.so'])
  const names = ['libubm5_jni_echo.so', 'libreactnative.so', 'libhermesvm.so', 'libfbjni.so', 'libextra.so']
  const entries = new Map(names.map(name => [`lib/armeabi-v7a/${name}`, elf]))
  expect(validateAndroidApkAbi(entries, 'armeabi-v7a')).toBe(5)
  const wrong = Buffer.from(elf)
  wrong[4] = 2
  const malformed = new Map(entries)
  malformed.set('lib/armeabi-v7a/libextra.so', wrong)
  expect(() => validateAndroidApkAbi(malformed, 'armeabi-v7a')).toThrow('ELF class/machine')
  entries.delete('lib/armeabi-v7a/libubm5_jni_echo.so')
  expect(() => validateAndroidApkAbi(entries, 'armeabi-v7a')).toThrow('libubm5_jni_echo.so')
})

test('APK native graph rejects truncated ELF and missing transitive bundled dependencies', () => {
  const { validateAndroidApkAbi } = require('../scripts/ci/check-android-apk-abi')
  const graph = () =>
    new Map(
      ['libubm5_jni_echo.so', 'libreactnative.so', 'libhermesvm.so', 'libfbjni.so'].map(name => [
        `lib/armeabi-v7a/${name}`,
        sharedElf(['libc.so'])
      ])
    )
  const truncated = graph()
  truncated.set('lib/armeabi-v7a/libextra.so', sharedElf().subarray(0, 20))
  expect(() => validateAndroidApkAbi(truncated, 'armeabi-v7a')).toThrow(/truncated/i)
  const transitive = graph()
  transitive.set('lib/armeabi-v7a/libreactnative.so', sharedElf(['libfirst.so']))
  transitive.set('lib/armeabi-v7a/libfirst.so', sharedElf(['libc++_shared.so']))
  expect(() => validateAndroidApkAbi(transitive, 'armeabi-v7a')).toThrow(/libc\+\+_shared/i)
  transitive.set('lib/arm64-v8a/libc++_shared.so', sharedElf([], 2, 183))
  expect(() => validateAndroidApkAbi(transitive, 'armeabi-v7a')).toThrow(/libc\+\+_shared/i)
  transitive.set('lib/armeabi-v7a/libc++_shared.so', sharedElf(['libc.so', 'libm.so', 'libdl.so']))
  expect(validateAndroidApkAbi(transitive, 'armeabi-v7a')).toBe(6)
  transitive.set('lib/armeabi-v7a/libfirst.so', sharedElf(['libandroid_runtime.so']))
  expect(() => validateAndroidApkAbi(transitive, 'armeabi-v7a')).toThrow(/libandroid_runtime/i)
})

test('APK graph rejects malformed load/dynamic/string tables and duplicate ZIP members', () => {
  const { validateAndroidApkAbi, collectAndroidApkAbiEntries } = require('../scripts/ci/check-android-apk-abi')
  const names = ['libubm5_jni_echo.so', 'libreactnative.so', 'libhermesvm.so', 'libfbjni.so']
  for (const mutate of [
    bytes => bytes.writeUInt32LE(0xfffffff0, 28),
    bytes => bytes.writeUInt32LE(2048, 52 + 16),
    bytes => bytes.writeUInt32LE(3, 52 + 28),
    bytes => bytes.writeUInt32LE(0, 52),
    bytes => bytes.writeUInt32LE(0, 52 + 32),
    bytes => bytes.writeUInt32LE(0xfffffff0, 256 + 4),
    bytes => bytes.writeUInt32LE(9999, 256 + 16 + 4),
    bytes => bytes.writeUInt32LE(1, 256 + 24),
    bytes => bytes.fill(65, 768)
  ]) {
    const bad = sharedElf(['libc.so'])
    mutate(bad)
    const entries = new Map(names.map(name => [`lib/armeabi-v7a/${name}`, sharedElf()]))
    entries.set('lib/armeabi-v7a/libextra.so', bad)
    expect(() => validateAndroidApkAbi(entries, 'armeabi-v7a')).toThrow()
  }
  expect(() =>
    collectAndroidApkAbiEntries(['lib/armeabi-v7a/libx.so', 'lib/armeabi-v7a/libx.so'], 'armeabi-v7a', () =>
      sharedElf()
    )
  ).toThrow(/duplicate/i)
  const arm64 = new Map(names.map(name => [`lib/arm64-v8a/${name}`, sharedElf(['libc.so'], 2, 183)]))
  expect(validateAndroidApkAbi(arm64, 'arm64-v8a')).toBe(4)
})
