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
  writeFile(root, 'package.json', JSON.stringify({ name: 'unified-ble-manager', version: '5.0.0-rc.0' }))
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
  return root
}

describe('build fingerprint (F23)', () => {
  test('generate is deterministic across runs', () => {
    const root = fixtureRoot()
    const first = generateBuildFingerprint(root)
    const second = generateBuildFingerprint(root)
    expect(second).toEqual(first)
    expect(first.package).toEqual({ name: 'unified-ble-manager', version: '5.0.0-rc.0' })
    expect(first.contractRevision).toBe('C-UBM.0.1.2-DRAFT')
    expect(first.files['src/a.ts']).toMatch(/^[0-9a-f]{64}$/)
    expect(first.files['android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so']).toMatch(/^[0-9a-f]{64}$/)
    expect(first.native.android).toEqual([
      {
        abi: 'arm64-v8a',
        file: 'android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so',
        sha256: first.files['android/src/main/jniLibs/arm64-v8a/libubm5_jni_echo.so'],
        bytes: 8
      }
    ])
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
})
