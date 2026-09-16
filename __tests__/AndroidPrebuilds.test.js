// __tests__/AndroidPrebuilds.test.js
//
// F01: packed consumers load the committed release prebuilts — they never
// skip the Rust library. This offline test (no NDK, no Rust) asserts the
// committed tree's coherence: build-identity.txt exists, names exactly the
// app ABI list, and every listed .so exists with matching bytes + sha256.
// Refresh via android/refresh-prebuilt-jniLibs.sh (maintainer step).

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const prebuiltDir = path.join(root, 'android', 'src', 'main', 'jniLibs')
const identityFile = path.join(prebuiltDir, 'build-identity.txt')
const EXPECTED_ABIS = ['arm64-v8a', 'x86_64']
const EXPECTED_FILE = 'libubm5_jni_echo.so'

function sha256(filePath) {
  return crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex')
}

describe('committed Android prebuilts (F01)', () => {
  test('build-identity.txt names exactly the app ABI list', () => {
    expect(fs.existsSync(identityFile)).toBe(true)
    const lines = fs.readFileSync(identityFile, 'utf8').split('\n')
    const entries = []
    for (const line of lines) {
      const match = /^abi=(\S+) sha256=([0-9a-f]{64}) bytes=(\d+) file=(\S+)$/.exec(line)
      if (match !== null) entries.push({ abi: match[1], sha256: match[2], bytes: Number(match[3]), file: match[4] })
    }
    expect(entries.map(entry => entry.abi).sort()).toEqual([...EXPECTED_ABIS].sort())
    for (const entry of entries) {
      expect(entry.file).toBe(EXPECTED_FILE)
      expect(entry.bytes).toBeGreaterThan(0)
    }
    expect(lines.some(line => line === 'profile=release')).toBe(true)
  })

  test('every listed prebuilt exists with matching bytes and sha256', () => {
    const lines = fs.readFileSync(identityFile, 'utf8').split('\n')
    for (const line of lines) {
      const match = /^abi=(\S+) sha256=([0-9a-f]{64}) bytes=(\d+) file=(\S+)$/.exec(line)
      if (match === null) continue
      const [, abi, expectedSha, expectedBytes, file] = match
      const absolute = path.join(prebuiltDir, abi, file)
      expect(fs.existsSync(absolute)).toBe(true)
      expect(fs.statSync(absolute).size).toBe(Number(expectedBytes))
      expect(sha256(absolute)).toBe(expectedSha)
    }
  })

  test('no other files ride the prebuilt tree', () => {
    const actual = []
    const visit = directory => {
      for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
        const absolute = path.join(directory, entry.name)
        if (entry.isDirectory()) {
          visit(absolute)
          continue
        }
        actual.push(path.relative(prebuiltDir, absolute).split(path.sep).join('/'))
      }
    }
    visit(prebuiltDir)
    expect(actual.sort()).toEqual(
      ['build-identity.txt', ...EXPECTED_ABIS.map(abi => `${abi}/${EXPECTED_FILE}`)].sort()
    )
  })
})
