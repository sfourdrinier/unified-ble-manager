// __tests__/AndroidPrebuilds.test.js
//
// F01: packed consumers load the committed release prebuilts — they never
// skip the Rust library. This offline test (no NDK, no Rust) asserts the
// committed tree's coherence: build-identity.json (PR210-18; it replaced
// build-identity.txt) exists, names exactly the app ABI list, and every
// listed .so exists with matching bytes + sha256. Whether the prebuilts were
// built from the CURRENT Rust sources is the publish gate
// (`native-build-identity.js --check-android-prebuilts`), not this suite:
// every Rust edit legitimately stales them until a maintainer refreshes via
// android/refresh-prebuilt-jniLibs.sh.

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const prebuiltDir = path.join(root, 'android', 'src', 'main', 'jniLibs')
const identityFile = path.join(prebuiltDir, 'build-identity.json')
const EXPECTED_ABIS = ['arm64-v8a', 'x86_64']
const EXPECTED_FILE = 'libubm5_jni_echo.so'

function identityEntries() {
  const record = JSON.parse(fs.readFileSync(identityFile, 'utf8'))
  expect(record.schema).toBe('ubm-android-jnilibs-identity/1')
  return record
}

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
  test('build-identity.json names exactly the app ABI list', () => {
    expect(fs.existsSync(identityFile)).toBe(true)
    const record = identityEntries()
    expect(record.abis.map(entry => entry.abi).sort()).toEqual([...EXPECTED_ABIS].sort())
    for (const entry of record.abis) {
      expect(entry.file).toBe(EXPECTED_FILE)
      expect(entry.bytes).toBeGreaterThan(0)
    }
    expect(record.profile).toBe('release')
  })

  test('every listed prebuilt exists with matching bytes and sha256', () => {
    for (const entry of identityEntries().abis) {
      const absolute = path.join(prebuiltDir, entry.abi, entry.file)
      expect(fs.existsSync(absolute)).toBe(true)
      expect(fs.statSync(absolute).size).toBe(entry.bytes)
      expect(sha256(absolute)).toBe(entry.sha256)
    }
  })

  test('every shipped .so is 16 KB page-aligned (Android 15+)', () => {
    let checked = 0
    for (const entry of identityEntries().abis) {
      const aligns = loadSegmentAlignments(path.join(prebuiltDir, entry.abi, entry.file))
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
      ['build-identity.json', ...EXPECTED_ABIS.map(abi => `${abi}/${EXPECTED_FILE}`)].sort()
    )
  })
})

describe('committed Android prebuilts seal (D1 / finding 157)', () => {
  const { execFileSync } = require('child_process')

  const bridgeFile = path.join(root, 'android', 'src', 'main', 'java', 'com', 'ubm', 'core', 'MobileCoreBridge.java')
  const JNI_PREFIX = 'Java_com_ubm_core_MobileCoreBridge_'

  /** Every JNI entrypoint the Kotlin side declares (single-owned by the Rust cdylib). */
  function kotlinDeclaredSymbols() {
    const source = fs.readFileSync(bridgeFile, 'utf8')
    const names = new Set()
    for (const match of source.matchAll(/public static native \S+ (\w+)\(/g)) {
      names.add(`${JNI_PREFIX}${match[1]}`)
    }
    expect(names.size).toBeGreaterThan(0)
    return names
  }

  /**
   * Defined dynamic symbols of an .so, read straight from its ELF dynamic
   * segment (PT_DYNAMIC → DT_SYMTAB/DT_STRTAB with the DT_HASH or DT_GNU_HASH
   * symbol count). Pure JS so the seal runs in every gate with no NDK; the
   * reader was cross-validated byte-for-byte against NDK llvm-nm
   * (`llvm-nm -D --defined-only`) on both shipped ABIs.
   */
  function definedDynamicSymbols(filePath) {
    const bytes = fs.readFileSync(filePath)
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const magic = [0x7f, 0x45, 0x4c, 0x46]
    for (let i = 0; i < magic.length; i += 1) {
      if (view.getUint8(i) !== magic[i]) throw new Error(`${filePath}: not an ELF file`)
    }
    if (view.getUint8(4) !== 2) throw new Error(`${filePath}: not 64-bit ELF`)
    if (view.getUint8(5) !== 1) throw new Error(`${filePath}: not little-endian ELF`)
    const phoff = Number(view.getBigUint64(32, true))
    const phentsize = view.getUint16(54, true)
    const phnum = view.getUint16(56, true)
    const loads = []
    let dynamic = null
    for (let i = 0; i < phnum; i += 1) {
      const base = phoff + i * phentsize
      const type = view.getUint32(base, true)
      const offset = Number(view.getBigUint64(base + 8, true))
      const vaddr = Number(view.getBigUint64(base + 16, true))
      const filesz = Number(view.getBigUint64(base + 32, true))
      if (type === 1) loads.push({ offset, vaddr, filesz })
      if (type === 2) dynamic = { offset, filesz }
    }
    if (dynamic === null) throw new Error(`${filePath}: no PT_DYNAMIC segment`)
    const toOffset = address => {
      for (const segment of loads) {
        if (address >= segment.vaddr && address < segment.vaddr + segment.filesz) {
          return address - segment.vaddr + segment.offset
        }
      }
      throw new Error(`${filePath}: dynamic address ${address.toString(16)} not mapped`)
    }
    let strtab = 0
    let strsz = 0
    let symtab = 0
    let hash = 0
    let gnuHash = 0
    for (let off = dynamic.offset; ; off += 16) {
      const tag = Number(view.getBigInt64(off, true))
      const value = Number(view.getBigUint64(off + 8, true))
      if (tag === 0) break
      if (tag === 5) strtab = value
      else if (tag === 10) strsz = value
      else if (tag === 6) symtab = value
      else if (tag === 4) hash = value
      else if (tag === 0x6ffffef5) gnuHash = value
      if (off > dynamic.offset + dynamic.filesz) throw new Error(`${filePath}: dynamic section overrun`)
    }
    if (strtab === 0 || symtab === 0 || (hash === 0 && gnuHash === 0)) {
      throw new Error(`${filePath}: dynamic symbol tables missing`)
    }
    let symbolCount = 0
    if (hash !== 0) {
      symbolCount = view.getUint32(toOffset(hash) + 4, true)
    } else {
      const base = toOffset(gnuHash)
      const nbuckets = view.getUint32(base, true)
      const symoffset = view.getUint32(base + 4, true)
      const bloomSize = view.getUint32(base + 8, true)
      const buckets = base + 16 + bloomSize * 8
      const chain = buckets + nbuckets * 4
      let highest = symoffset
      for (let bucket = 0; bucket < nbuckets; bucket += 1) {
        let symbol = view.getUint32(buckets + bucket * 4, true)
        if (symbol === 0) continue
        for (;;) {
          if (symbol > highest) highest = symbol
          const word = view.getUint32(chain + (symbol - symoffset) * 4, true)
          if ((word & 1) !== 0) break
          symbol += 1
        }
      }
      symbolCount = highest + 1
    }
    const readCString = off => {
      let end = off
      while (bytes[end] !== 0) end += 1
      return Buffer.from(bytes.subarray(off, end)).toString('utf8')
    }
    const names = new Set()
    const symOff = toOffset(symtab)
    const strOff = toOffset(strtab)
    for (let i = 0; i < symbolCount; i += 1) {
      const base = symOff + i * 24
      const nameOff = view.getUint32(base, true)
      const section = view.getUint16(base + 6, true)
      if (section !== 0 && nameOff < strsz) names.add(readCString(strOff + nameOff))
    }
    return names
  }

  test('every MobileCoreBridge JNI symbol the Kotlin side declares exists in each committed .so', () => {
    const declared = kotlinDeclaredSymbols()
    expect(declared.size).toBeGreaterThan(10)
    for (const entry of identityEntries().abis) {
      const symbols = definedDynamicSymbols(path.join(prebuiltDir, entry.abi, entry.file))
      const missing = [...declared].filter(name => !symbols.has(name))
      expect(missing).toEqual([])
    }
  })

  test('native-build-identity --check-android-prebuilts passes for the committed prebuilts', () => {
    execFileSync(
      process.execPath,
      [path.join(root, 'scripts', 'release', 'native-build-identity.js'), '--root', root, '--check-android-prebuilts'],
      { stdio: 'pipe' }
    )
  })
})
