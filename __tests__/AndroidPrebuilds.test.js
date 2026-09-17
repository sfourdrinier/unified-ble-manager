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

// D2(iii): offline 16 KB page-size check (Android 15+ install requirement).
// Pure-JS ELF program-header walk — no readelf, no NDK. Both shipped ABIs
// are 64-bit little-endian; anything else fails closed (unsupported, not
// assumed-aligned). Every PT_LOAD segment must carry p_align >= 0x4000.
function loadSegmentAlignments(filePath) {
  const bytes = fs.readFileSync(filePath)
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const magic = [0x7f, 0x45, 0x4c, 0x46]
  for (let i = 0; i < magic.length; i += 1) {
    if (view.getUint8(i) !== magic[i]) throw new Error(`${filePath}: not an ELF file`)
  }
  if (view.getUint8(4) !== 2) throw new Error(`${filePath}: not 64-bit ELF (EI_CLASS=${view.getUint8(4)})`)
  if (view.getUint8(5) !== 1) throw new Error(`${filePath}: not little-endian ELF (EI_DATA=${view.getUint8(5)})`)
  const phoff = Number(view.getBigUint64(32, true))
  const phentsize = view.getUint16(54, true)
  const phnum = view.getUint16(56, true)
  const aligns = []
  for (let i = 0; i < phnum; i += 1) {
    const base = phoff + i * phentsize
    const type = view.getUint32(base, true)
    if (type === 1) aligns.push(Number(view.getBigUint64(base + 48, true)))
  }
  return aligns
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

  test('every shipped .so is 16 KB page-aligned (Android 15+)', () => {
    const lines = fs.readFileSync(identityFile, 'utf8').split('\n')
    let checked = 0
    for (const line of lines) {
      const match = /^abi=(\S+) sha256=[0-9a-f]{64} bytes=\d+ file=(\S+)$/.exec(line)
      if (match === null) continue
      const [, abi, file] = match
      const aligns = loadSegmentAlignments(path.join(prebuiltDir, abi, file))
      expect(aligns.length).toBeGreaterThan(0)
      for (const align of aligns) {
        expect(align).toBeGreaterThanOrEqual(16384)
      }
      checked += 1
    }
    expect(checked).toBe(EXPECTED_ABIS.length)
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
