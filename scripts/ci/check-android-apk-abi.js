// Check the complete installed native graph, not merely UBM's packaged slice.
const fs = require('node:fs')
const { execFileSync } = require('node:child_process')

// Public Android NDK system libraries; everything else (notably libc++_shared)
// belongs in the selected ABI directory. Private platform libraries are not
// an acceptable substitute for a missing packaged dependency.
const ANDROID_SYSTEM_LIBRARIES = new Set([
  'libc.so',
  'libm.so',
  'libdl.so',
  'liblog.so',
  'libandroid.so',
  'libz.so',
  'libEGL.so',
  'libGLESv1_CM.so',
  'libGLESv2.so',
  'libGLESv3.so',
  'libOpenSLES.so',
  'libjnigraphics.so',
  'libmediandk.so',
  'libcamera2ndk.so',
  'libvulkan.so',
  'libnativewindow.so'
])

function elfDependencies(bytes, expected, name) {
  const fail = message => {
    throw new Error(`APK library ${name}: ${message}`)
  }
  const is32 = expected.elfClass === 1
  const headerSize = is32 ? 52 : 64
  const programSize = is32 ? 32 : 56
  const wordSize = is32 ? 4 : 8
  const range = (offset, size) => {
    if (
      !Number.isSafeInteger(offset) ||
      !Number.isSafeInteger(size) ||
      offset < 0 ||
      size < 0 ||
      offset > bytes.length ||
      size > bytes.length - offset
    )
      fail('truncated or invalid ELF range')
  }
  range(0, headerSize)
  if (
    !bytes.subarray(0, 4).equals(Buffer.from([0x7f, 69, 76, 70])) ||
    bytes[4] !== expected.elfClass ||
    bytes[5] !== 1 ||
    bytes[6] !== 1 ||
    bytes.readUInt16LE(16) !== 3 ||
    bytes.readUInt16LE(18) !== expected.machine ||
    bytes.readUInt32LE(20) !== 1
  )
    fail('incompatible ELF class/machine/version')
  const word = offset => {
    range(offset, wordSize)
    if (is32) return bytes.readUInt32LE(offset)
    const value = bytes.readBigUInt64LE(offset)
    if (value > BigInt(Number.MAX_SAFE_INTEGER)) fail('ELF integer exceeds safe address range')
    return Number(value)
  }
  if (bytes.readUInt16LE(is32 ? 40 : 52) !== headerSize) fail('invalid ELF header size')
  const table = word(is32 ? 28 : 32)
  const entrySize = bytes.readUInt16LE(is32 ? 42 : 54)
  const count = bytes.readUInt16LE(is32 ? 44 : 56)
  if (table < headerSize || entrySize !== programSize || count === 0) fail('invalid program-header table')
  range(table, count * entrySize)
  const loads = []
  let dynamic
  for (let index = 0; index < count; index++) {
    const entry = table + index * entrySize
    const type = bytes.readUInt32LE(entry)
    const offset = word(entry + (is32 ? 4 : 8))
    const address = word(entry + (is32 ? 8 : 16))
    const size = word(entry + (is32 ? 16 : 32))
    const memorySize = word(entry + (is32 ? 20 : 40))
    const alignment = word(entry + (is32 ? 28 : 48))
    range(offset, size)
    if (memorySize < size || !Number.isSafeInteger(address + memorySize)) fail('invalid segment memory size')
    if (alignment > 1 && (!/^10*$/u.test(alignment.toString(2)) || offset % alignment !== address % alignment))
      fail('invalid segment alignment')
    if (type === 1) loads.push({ offset, address, size })
    if (type === 2) {
      if (dynamic) fail('duplicate dynamic segment')
      dynamic = { offset, address, size }
    }
  }
  if (loads.length === 0 || !dynamic || dynamic.size === 0 || dynamic.size % (wordSize * 2) !== 0) {
    fail('missing loadable dynamic ELF segments')
  }
  const fileOffset = (address, size) => {
    const matches = loads.filter(
      load =>
        address >= load.address && address - load.address <= load.size && size <= load.size - (address - load.address)
    )
    if (matches.length !== 1) fail('dynamic data is outside a unique file-backed load segment')
    return matches[0].offset + address - matches[0].address
  }
  if (fileOffset(dynamic.address, dynamic.size) !== dynamic.offset) fail('inconsistent dynamic segment mapping')
  const needed = []
  let stringAddress,
    stringSize,
    terminated = false
  for (let offset = dynamic.offset; offset < dynamic.offset + dynamic.size; offset += wordSize * 2) {
    const tag = word(offset)
    const value = word(offset + wordSize)
    if (tag === 0) {
      terminated = true
      break
    }
    if (tag === 1) needed.push(value)
    if (tag === 5) {
      if (stringAddress !== undefined) fail('duplicate dynamic string table')
      stringAddress = value
    }
    if (tag === 10) {
      if (stringSize !== undefined) fail('duplicate dynamic string size')
      stringSize = value
    }
  }
  if (!terminated || stringAddress === undefined || stringSize === undefined || stringSize === 0) {
    fail('missing terminated dynamic/string table')
  }
  const strings = fileOffset(stringAddress, stringSize)
  return needed.map(index => {
    if (index >= stringSize) fail('DT_NEEDED string offset outside table')
    const end = bytes.indexOf(0, strings + index)
    if (end < strings + index || end >= strings + stringSize) fail('unterminated DT_NEEDED string')
    const dependency = bytes.subarray(strings + index, end).toString('utf8')
    if (!/^[A-Za-z0-9_+.-]+\.so$/u.test(dependency)) fail('invalid DT_NEEDED library name')
    return dependency
  })
}

function collectAndroidApkAbiEntries(names, abi, readEntry) {
  const entries = new Map()
  for (const name of names.filter(value => value.startsWith(`lib/${abi}/`) && value.endsWith('.so'))) {
    if (entries.has(name)) throw new Error(`APK has duplicate native ZIP member ${name}`)
    entries.set(name, readEntry(name))
  }
  return entries
}

function validateAndroidApkAbi(entries, abi) {
  const expected = { 'armeabi-v7a': { elfClass: 1, machine: 40 }, 'arm64-v8a': { elfClass: 2, machine: 183 } }[abi]
  if (!expected) throw new Error(`Unsupported acceptance ABI: ${abi}`)
  const prefix = `lib/${abi}/`
  const libraries = [...entries.keys()].filter(name => name.startsWith(prefix) && name.endsWith('.so'))
  for (const name of ['libubm5_jni_echo.so', 'libreactnative.so', 'libfbjni.so']) {
    if (!libraries.includes(prefix + name)) throw new Error(`APK ${abi} native graph is missing ${name}`)
  }
  if (!['libhermes.so', 'libhermesvm.so'].some(name => libraries.includes(prefix + name))) {
    throw new Error(`APK ${abi} native graph is missing Hermes`)
  }
  for (const name of libraries) {
    if (name.slice(prefix.length).includes('/')) throw new Error(`Invalid APK native library path ${name}`)
    for (const dependency of elfDependencies(entries.get(name), expected, name)) {
      if (!ANDROID_SYSTEM_LIBRARIES.has(dependency) && !entries.has(prefix + dependency)) {
        throw new Error(`APK library ${name} needs missing bundled dependency ${dependency} for ${abi}`)
      }
    }
  }
  return libraries.length
}

if (require.main === module) {
  const [apk, abi = 'armeabi-v7a'] = process.argv.slice(2)
  if (!apk || !fs.existsSync(apk)) throw new Error('Expected an existing APK path')
  const names = execFileSync('unzip', ['-Z1', apk], { encoding: 'utf8' }).trim().split(/\r?\n/u)
  const entries = collectAndroidApkAbiEntries(names, abi, name =>
    execFileSync('unzip', ['-p', apk, name], { maxBuffer: 256 * 1024 * 1024 })
  )
  const count = validateAndroidApkAbi(entries, abi)
  console.log(
    `Android APK native graph: ${abi}, ${count} compatible ELF objects, UBM + React Native + Hermes present (${apk})`
  )
}

module.exports = { validateAndroidApkAbi, collectAndroidApkAbiEntries }
