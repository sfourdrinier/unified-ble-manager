// __tests__/BuildFingerprint.test.js
//
// F23: the build fingerprint replaces the consumer max-mtime freshness
// heuristic. `lib/ubm-build-fingerprint.json` seals the exact source +
// contract + config + native-artifact input set that produced `lib/`; the
// check fails on ANY drift (changed bytes, added files, removed files,
// tampered native binaries, tampered seal) and can never be satisfied by
// touching an unrelated output file.

const fs = require('fs')
const os = require('os')
const path = require('path')

const {
  generateBuildFingerprint,
  writeBuildFingerprint,
  checkBuildFingerprint
} = require('../scripts/release/generate-build-fingerprint')

function writeFile(root, relative, content) {
  const absolute = path.join(root, relative)
  fs.mkdirSync(path.dirname(absolute), { recursive: true })
  fs.writeFileSync(absolute, content)
  return absolute
}

function fixtureRoot() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-fingerprint-'))
  writeFile(root, 'package.json', JSON.stringify({ name: 'unified-ble-manager', version: '5.0.0-rc.10' }))
  writeFile(root, 'src/a.ts', 'export const a = 1\n')
  writeFile(root, 'src/b.ts', 'export const b = 2\n')
  writeFile(root, 'contracts/frozen.txt', 'C-UBM.0.1.2-DRAFT\n')
  writeFile(root, 'crates/ubm-core/src/contracts.rs', 'pub const CONTRACT_REVISION: &str = "C-UBM.0.1.2-DRAFT";\n')
  writeFile(root, 'Cargo.toml', '[workspace]\n')
  writeFile(root, 'Cargo.lock', '# lock\n')
  writeFile(root, 'rust-toolchain.toml', 'channel = "1.98.1"\n')
  writeFile(
    root,
    'android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so',
    Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x01, 0x02, 0x03, 0x04])
  )
  writeFile(root, '__tests__/a.test.js', "test('a', () => {})\n")
  writeFile(root, 'lib/.gitkeep', '')
  // D2(iv) extended identities: deployment floors, per-crate features (jni
  // declares [features], uniffi relies on the implicit default), and a
  // staged Apple slice pair with LibraryIdentifiers.
  writeFile(
    root,
    'unified-ble-manager.podspec',
    'Pod::Spec.new do |s|\n  s.platforms    = { :ios => "16.4", :tvos => "16.4" }\nend\n'
  )
  writeFile(root, 'android/gradle.properties', 'BlePlx_minSdkVersion=24\n')
  writeFile(root, 'bindings/jni/Cargo.toml', '[package]\nname = "ubm5_jni_echo"\n\n[features]\nbeta = []\nalpha = []\n')
  writeFile(root, 'bindings/uniffi/Cargo.toml', '[package]\nname = "ubm5_uniffi_echo"\n')
  // PR210-18: complete (minimal) binding crates, so the seal can record the
  // per-binding sourceDigest + bindingSchema from the identity library.
  writeFile(root, 'bindings/ubm_build_identity.rs', '// helper\n')
  writeFile(root, 'bindings/napi/Cargo.toml', '[package]\nname = "ubm5_napi_echo"\n')
  writeFile(root, 'bindings/napi/src/lib.rs', '// napi\n')
  writeFile(root, 'bindings/napi/src/dispatch.rs', '// dispatch\n')
  writeFile(root, 'bindings/jni/src/lib.rs', '// jni\n')
  writeFile(root, 'bindings/uniffi/src/lib.rs', '// uniffi\n')
  writeFile(root, 'bindings/uniffi/src/ubm_echo.udl', 'namespace ubm_echo {};\n')
  writeFile(root, 'bindings/uniffi/generated/swift/ubm_echo.swift', '// swift\n')
  writeFile(root, 'bindings/uniffi/generated/swift/ubm_echoFFI.h', '// h\n')
  writeFile(root, 'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap', 'module ubm_echoFFI {}\n')
  writeFile(
    root,
    'ios/RustCore/RustCore.xcframework/Info.plist',
    '<?xml version="1.0"?>\n<plist><dict><key>AvailableLibraries</key><array>' +
      '<dict><key>LibraryIdentifier</key><string>ios-arm64</string></dict>' +
      '<dict><key>LibraryIdentifier</key><string>ios-arm64-simulator</string></dict>' +
      '</array></dict></plist>\n'
  )
  writeFile(
    root,
    'ios/RustCore/RustCore.xcframework/ios-arm64/libubm5_uniffi_echo.a',
    Buffer.from([0x21, 0x3c, 0x61, 0x72, 0x63, 0x68, 0x3e, 0x0a])
  )
  const identity = require('../scripts/release/native-build-identity')
  const crypto = require('node:crypto')
  const hash = content => crypto.createHash('sha256').update(content).digest('hex')
  const androidBytes = fs.readFileSync(path.join(root, 'android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so'))
  const abis = identity.ANDROID_DECLARED_ABIS.map(entry => {
    writeFile(root, `android/src/main/jniLibs/${entry.abi}/libubm5_jni_echo.so`, androidBytes)
    return { ...entry, file: 'libubm5_jni_echo.so', sha256: hash(androidBytes), bytes: androidBytes.length }
  })
  writeFile(
    root,
    'android/src/main/jniLibs/build-identity.json',
    JSON.stringify({
      schema: identity.ANDROID_PREBUILT_SCHEMA,
      binding: 'jni',
      contractRevision: identity.readContractRevision(root),
      ...identity.computeBindingIdentity(root, 'jni'),
      ndk: '27.1.12297006',
      abis
    })
  )
  const appleBytes = Buffer.from([0x21, 0x3c, 0x61, 0x72, 0x63, 0x68, 0x3e, 0x0a])
  const libraries = identity.APPLE_DECLARED_LIBRARIES.map(entry => {
    writeFile(root, `ios/RustCore/RustCore.xcframework/${entry.libraryIdentifier}/libubm5_uniffi_echo.a`, appleBytes)
    return { ...entry, libraryPath: 'libubm5_uniffi_echo.a', sha256: hash(appleBytes), bytes: appleBytes.length }
  })
  writeFile(
    root,
    'ios/RustCore/RustCore.xcframework/Info.plist',
    '<?xml version="1.0"?><plist><dict><key>AvailableLibraries</key><array>' +
      libraries
        .map(entry => `<dict><key>LibraryIdentifier</key><string>${entry.libraryIdentifier}</string></dict>`)
        .join('') +
      '</array></dict></plist>'
  )
  writeFile(
    root,
    'ios/RustCore/build-identity.json',
    JSON.stringify({
      schema: identity.APPLE_STAGING_SCHEMA,
      binding: 'uniffi',
      contractRevision: identity.readContractRevision(root),
      ...identity.computeBindingIdentity(root, 'uniffi'),
      xcodebuild: 'Xcode producer-test',
      libraries,
      infoPlistSha256: hash(fs.readFileSync(path.join(root, 'ios/RustCore/RustCore.xcframework/Info.plist')))
    })
  )
  return root
}

describe('build fingerprint (F23)', () => {
  test('SDK provenance comes from staged producers, never the sealing machine', () => {
    const root = fixtureRoot()
    const seal = generateBuildFingerprint(root)
    expect(seal.toolchain).toEqual({ rust: '1.98.1', ndk: '27.1.12297006', xcode: 'Xcode producer-test' })
  })

  test.each(['android/src/main/jniLibs/build-identity.json', 'ios/RustCore/build-identity.json'])(
    'staged artifacts cannot silently lose producer metadata: %s',
    relative => {
      const root = fixtureRoot()
      fs.unlinkSync(path.join(root, relative))
      expect(() => generateBuildFingerprint(root)).toThrow(/build-identity/)
    }
  )

  test.each([
    ['android/src/main/jniLibs/build-identity.json', 'ndk'],
    ['ios/RustCore/build-identity.json', 'xcodebuild']
  ])('rejects malformed producer field %s %s', (relative, field) => {
    const root = fixtureRoot()
    const record = JSON.parse(fs.readFileSync(path.join(root, relative), 'utf8'))
    record[field] = ''
    writeFile(root, relative, JSON.stringify(record))
    expect(() => generateBuildFingerprint(root)).toThrow(/producer/)
  })

  test('unstaged development artifacts have no inferred SDK provenance', () => {
    const root = fixtureRoot()
    fs.rmSync(path.join(root, 'android/src/main/jniLibs'), { recursive: true })
    fs.rmSync(path.join(root, 'ios/RustCore'), { recursive: true })
    expect(generateBuildFingerprint(root).toolchain).toEqual({ rust: '1.98.1', ndk: null, xcode: null })
  })

  test('changed producer provenance invalidates an existing seal', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    const relative = 'android/src/main/jniLibs/build-identity.json'
    const record = JSON.parse(fs.readFileSync(path.join(root, relative), 'utf8'))
    record.ndk = '28.0.0'
    writeFile(root, relative, JSON.stringify(record))
    expect(() => checkBuildFingerprint(root)).toThrow(/toolchain identity changed/)
  })

  test('resealing cannot bless corrupted staged Apple bytes', () => {
    const root = fixtureRoot()
    writeFile(root, 'ios/RustCore/RustCore.xcframework/ios-arm64/libubm5_uniffi_echo.a', 'corrupt')
    expect(() => generateBuildFingerprint(root)).toThrow(/slice ios-arm64.*substituted or corrupted/)
  })
  test('generate is deterministic across runs', () => {
    const root = fixtureRoot()
    const first = generateBuildFingerprint(root)
    const second = generateBuildFingerprint(root)
    expect(second).toEqual(first)
    expect(first.package).toEqual({ name: 'unified-ble-manager', version: '5.0.0-rc.10' })
    expect(first.contractRevision).toBe('C-UBM.0.1.2-DRAFT')
    expect(first.files['src/a.ts']).toMatch(/^[0-9a-f]{64}$/)
    expect(first.files['android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so']).toMatch(/^[0-9a-f]{64}$/)
    expect(first.native.android).toEqual(
      ['arm64-v8a', 'x86_64'].map(abi => ({
        abi,
        file: `android/src/main/jniLibs/${abi}/libubm5_jni_echo.so`,
        sha256: first.files[`android/src/main/jniLibs/${abi}/libubm5_jni_echo.so`],
        bytes: 8
      }))
    )
    expect(first.fingerprint).toMatch(/^[0-9a-f]{64}$/)
  })

  test('check passes on a fresh seal and the seal lives in lib/', () => {
    const root = fixtureRoot()
    const written = writeBuildFingerprint(root)
    expect(written).toBe(path.join(root, 'lib', 'ubm-build-fingerprint.json'))
    expect(() => checkBuildFingerprint(root)).not.toThrow()
  })

  test('modify one source without resealing fails and names the file', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(root, 'src/a.ts', 'export const a = 2\n')
    expect(() => checkBuildFingerprint(root)).toThrow(/src\/a\.ts/)
  })

  test('touching an unrelated output does not clear source drift (F23 regression)', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(root, 'src/a.ts', 'export const a = 2\n')
    const unrelated = writeFile(root, 'lib/other.js', 'console.log("newer")\n')
    const future = new Date(Date.now() + 60_000)
    fs.utimesSync(unrelated, future, future)
    fs.utimesSync(path.join(root, 'lib', 'ubm-build-fingerprint.json'), future, future)
    expect(() => checkBuildFingerprint(root)).toThrow(/src\/a\.ts/)
  })

  test('tampered native bytes fail and name the artifact', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(
      root,
      'android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so',
      Buffer.from([0x7f, 0x45, 0x4c, 0x46, 0x09, 0x09, 0x09, 0x09])
    )
    expect(() => checkBuildFingerprint(root)).toThrow(/libubm5_jni_echo\.so/)
  })

  test('a new source file fails closed (fail-closed on new inputs)', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(root, 'src/c.ts', 'export const c = 3\n')
    expect(() => checkBuildFingerprint(root)).toThrow(/src\/c\.ts/)
  })

  test('test-only edits do not stale the build', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(root, '__tests__/a.test.js', "test('b', () => {})\n")
    writeFile(root, 'src/a.test.ts', "test('x', () => {})\n")
    expect(() => checkBuildFingerprint(root)).not.toThrow()
  })

  test('tooling outputs and machine-local files are not inputs', () => {
    const root = fixtureRoot()
    writeFile(root, 'native/tauri/Cargo.lock', '# v1\n')
    writeFile(root, 'plugin/tsconfig.tsbuildinfo', '{}')
    writeBuildFingerprint(root)
    writeFile(root, 'native/tauri/Cargo.lock', '# v2 regenerated\n')
    writeFile(root, 'plugin/tsconfig.tsbuildinfo', '{"v":2}')
    writeFile(root, 'android/local.properties', 'sdk.dir=/x\n')
    writeFile(root, 'Pods/Manifest.lock', 'PODS: []\n')
    writeFile(root, 'debug.log', 'noise\n')
    writeFile(root, 'README.md', '# changed\n')
    expect(() => checkBuildFingerprint(root)).not.toThrow()
  })

  test('a tampered seal fails instead of verifying', () => {
    const root = fixtureRoot()
    const seal = writeBuildFingerprint(root)
    const parsed = JSON.parse(fs.readFileSync(seal, 'utf8'))
    parsed.files['src/a.ts'] = '0'.repeat(64)
    fs.writeFileSync(seal, JSON.stringify(parsed, null, 2))
    expect(() => checkBuildFingerprint(root)).toThrow(/seal|integrity|fingerprint/i)
  })

  test('a missing seal fails with a rebuild message', () => {
    const root = fixtureRoot()
    expect(() => checkBuildFingerprint(root)).toThrow(/prepack|rebuild|missing/i)
  })

  test('extended identities bind toolchain, features, targets, floors, apple slices', () => {
    const root = fixtureRoot()
    const seal = generateBuildFingerprint(root)
    expect(seal.toolchain.rust).toBe('1.98.1')
    expect(seal.features).toEqual({ jni: ['alpha', 'beta'], uniffi: ['default'] })
    expect(seal.deploymentMinimum).toEqual({ ios: '16.4', tvos: '16.4', androidMinSdk: 24 })
    expect(seal.targets.android).toEqual({ 'arm64-v8a': 'aarch64-linux-android', x86_64: 'x86_64-linux-android' })
    const slices = require('../scripts/release/native-build-identity')
      .APPLE_DECLARED_LIBRARIES.map(entry => entry.libraryIdentifier)
      .sort()
    expect(seal.targets.apple).toEqual(slices)
    expect(seal.native.apple.staged).toBe(true)
    expect(seal.native.apple.libraryIdentifiers).toEqual(slices)
    expect(seal.native.apple.slices).toHaveLength(4)
    expect(seal.native.apple.slices[0]).toMatchObject({
      slice: 'ios-arm64',
      file: 'ios/RustCore/RustCore.xcframework/ios-arm64/libubm5_uniffi_echo.a',
      bytes: 8
    })
  })

  test('unstaged apple framework seals empty without failing', () => {
    const root = fixtureRoot()
    fs.rmSync(path.join(root, 'ios'), { recursive: true, force: true })
    const seal = generateBuildFingerprint(root)
    expect(seal.native.apple).toEqual({ staged: false, slices: [], libraryIdentifiers: [] })
    expect(seal.targets.apple).toEqual([])
  })

  test('bindingSchema and sourceDigest are sealed per binding (T1 closed)', () => {
    const root = fixtureRoot()
    const identity = require('../scripts/release/native-build-identity')
    const seal = generateBuildFingerprint(root)
    for (const binding of ['napi', 'jni', 'uniffi']) {
      const expected = identity.computeBindingIdentity(root, binding)
      expect(seal.bindingSchema[binding]).toBe(expected.bindingSchema)
      expect(seal.sourceDigest[binding]).toBe(expected.sourceDigest)
    }
  })

  test('a binding schema edit fails the seal as identity drift', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(root, 'bindings/uniffi/generated/swift/ubm_echo.swift', '// regenerated\n')
    expect(() => checkBuildFingerprint(root)).toThrow(
      /staged uniffi artifacts were not built from these sources:\s+bindingSchema/
    )
  })

  test('the generator no longer declares bindingSchema unsealed', () => {
    const source = fs.readFileSync(
      path.join(__dirname, '..', 'scripts', 'release', 'generate-build-fingerprint.js'),
      'utf8'
    )
    expect(source).not.toMatch(/stays unsealed/)
  })

  test('desktop-core NAPI prebuilds from PREBUILDS.json join native.napi', () => {
    const root = fixtureRoot()
    expect(generateBuildFingerprint(root).native.napi).toEqual([])
    const entry = {
      backend: 'desktop-core',
      platform: 'darwin',
      arch: 'arm64',
      path: 'native/desktop-core/prebuilds/darwin-arm64/ubm_desktop_core.node',
      bytes: 3,
      sha256: 'c'.repeat(64)
    }
    writeFile(
      root,
      'native/PREBUILDS.json',
      JSON.stringify({ schemaVersion: 1, entries: [entry, { ...entry, backend: 'corebluetooth' }] })
    )
    expect(generateBuildFingerprint(root).native.napi).toEqual([entry])
  })

  test('identity drift fails closed and names the identity', () => {
    const root = fixtureRoot()
    writeBuildFingerprint(root)
    writeFile(root, 'android/gradle.properties', 'BlePlx_minSdkVersion=26\n')
    expect(() => checkBuildFingerprint(root)).toThrow(/deploymentMinimum/)
  })
})
