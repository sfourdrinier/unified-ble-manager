'use strict'

// scripts/native/native-status.js — F9: one `pnpm native:status` that reports
// whether every precompiled Rust artifact matches the current sources.
//
// Every digest comes from scripts/release/native-build-identity.js (required
// below, never reimplemented): sourceDigest/bindingSchema per binding plus
// the hash-chain checkers for the Apple staging and the committed Android
// jniLibs. The desktop prebuild check reads the sidecar the canonical
// builder wrote and compares its sealed digests, exactly like the loader
// does before mapping the binary.
//
// Usage:
//   node scripts/native/native-status.js [--root <dir>] [--only android,apple,desktop] [--json]
//
// One line per artifact (`fresh`, `stale`, `missing`, or `not-applicable`
// with the staged and current digests and the exact refresh command), exit
// non-zero when any applicable artifact is stale or missing. Rows that
// cannot be built on this host are listed as not-applicable, never skipped.

const crypto = require('node:crypto')
const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.resolve(__dirname, '..', '..')
const IDENTITY_SCRIPT = path.join(ROOT, 'scripts', 'release', 'native-build-identity.js')

const GROUPS = Object.freeze(['android', 'apple', 'desktop'])
const KNOWN_DESKTOP_DIRS = Object.freeze([
  'darwin-arm64',
  'darwin-x64',
  'linux-arm64',
  'linux-x64',
  'win32-arm64',
  'win32-x64'
])
const DESKTOP_ADDON = 'ubm_desktop_core.node'
const DESKTOP_SIDECAR = 'ubm_desktop_core.identity.json'
const DESKTOP_SIDECAR_SCHEMA = 'ubm-desktop-core-prebuild/1'

function resolveIdentity() {
  return require(process.env.UBM_NATIVE_IDENTITY_MODULE ?? IDENTITY_SCRIPT)
}

function firstLine(message) {
  return String(message).split('\n')[0]
}

function isHex64(value) {
  return typeof value === 'string' && /^[0-9a-f]{64}$/.test(value)
}

function defaultRefreshCommand(group, options = {}) {
  if (group === 'android') return 'sh android/refresh-prebuilt-jniLibs.sh'
  if (group === 'apple') return 'pnpm native:apple:prepare'
  if (group === 'desktop') {
    return `node scripts/ci/build-napi-addon.js --profile release --out native/desktop-core/prebuilds/${options.dir}/${DESKTOP_ADDON}`
  }
  throw new Error(`unknown native artifact group '${group}' (expected ${GROUPS.join(', ')})`)
}

function readRecord(file) {
  let text
  try {
    text = fs.readFileSync(file, 'utf8')
  } catch (error) {
    if (error.code === 'ENOENT') return { found: false }
    throw error
  }
  try {
    return { found: true, record: JSON.parse(text) }
  } catch (error) {
    throw new Error(`${file} is not valid JSON: ${firstLine(error.message)}`)
  }
}

function stagedDigests(record) {
  if (record === null || typeof record !== 'object') return null
  if (!isHex64(record.sourceDigest) || !isHex64(record.bindingSchema)) return null
  return { sourceDigest: record.sourceDigest, bindingSchema: record.bindingSchema }
}

function classifyAndroid(root, identity) {
  const file = path.join(root, 'android', 'src', 'main', 'jniLibs', 'build-identity.json')
  const current = identity.computeBindingIdentity(root, 'jni')
  const refresh = defaultRefreshCommand('android')
  let record
  try {
    const read = readRecord(file)
    if (!read.found) {
      return {
        group: 'android',
        name: 'android',
        state: 'missing',
        staged: null,
        current,
        refresh,
        detail: `missing ${file}`
      }
    }
    record = read.record
  } catch (error) {
    return {
      group: 'android',
      name: 'android',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: firstLine(error.message)
    }
  }
  const staged = stagedDigests(record)
  if (staged === null) {
    return {
      group: 'android',
      name: 'android',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: `${file} carries no sealed sourceDigest/bindingSchema`
    }
  }
  if (staged.sourceDigest !== current.sourceDigest || staged.bindingSchema !== current.bindingSchema) {
    return {
      group: 'android',
      name: 'android',
      state: 'stale',
      staged,
      current,
      refresh,
      detail: `sourceDigest staged ${staged.sourceDigest}, sources ${current.sourceDigest}`
    }
  }
  try {
    identity.checkAndroidPrebuilts(root)
  } catch (error) {
    return {
      group: 'android',
      name: 'android',
      state: 'stale',
      staged,
      current,
      refresh,
      detail: firstLine(error.message)
    }
  }
  return { group: 'android', name: 'android', state: 'fresh', staged, current, refresh, detail: null }
}

function classifyApple(root, identity, platform = process.platform) {
  if (platform !== 'darwin') {
    return {
      group: 'apple',
      name: 'apple',
      state: 'not-applicable',
      staged: null,
      current: null,
      refresh: null,
      detail: null,
      reason: 'needs macOS+Xcode (built by ios/build-rust-core.sh)'
    }
  }
  const file = path.join(root, 'ios', 'RustCore', 'build-identity.json')
  const current = identity.computeBindingIdentity(root, 'uniffi')
  const refresh = defaultRefreshCommand('apple')
  let record
  try {
    const read = readRecord(file)
    if (!read.found) {
      return {
        group: 'apple',
        name: 'apple',
        state: 'missing',
        staged: null,
        current,
        refresh,
        detail: `missing ${file}`
      }
    }
    record = read.record
  } catch (error) {
    return {
      group: 'apple',
      name: 'apple',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: firstLine(error.message)
    }
  }
  const staged = stagedDigests(record)
  if (staged === null) {
    return {
      group: 'apple',
      name: 'apple',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: `${file} carries no sealed sourceDigest/bindingSchema`
    }
  }
  if (staged.sourceDigest !== current.sourceDigest || staged.bindingSchema !== current.bindingSchema) {
    return {
      group: 'apple',
      name: 'apple',
      state: 'stale',
      staged,
      current,
      refresh,
      detail: `sourceDigest staged ${staged.sourceDigest}, sources ${current.sourceDigest}`
    }
  }
  try {
    identity.checkAppleStaging(root)
  } catch (error) {
    return { group: 'apple', name: 'apple', state: 'stale', staged, current, refresh, detail: firstLine(error.message) }
  }
  return { group: 'apple', name: 'apple', state: 'fresh', staged, current, refresh, detail: null }
}

function classifyDesktopDir(root, identity, dir) {
  const addon = path.join(root, 'native', 'desktop-core', 'prebuilds', dir, DESKTOP_ADDON)
  const sidecarFile = path.join(root, 'native', 'desktop-core', 'prebuilds', dir, DESKTOP_SIDECAR)
  const current = identity.computeBindingIdentity(root, 'napi')
  const refresh = defaultRefreshCommand('desktop', { dir })
  if (!fs.existsSync(addon) || !fs.existsSync(sidecarFile)) {
    return {
      group: 'desktop',
      name: 'desktop',
      state: 'missing',
      staged: null,
      current,
      refresh,
      detail: `missing ${addon} or its sidecar`
    }
  }
  let sidecar
  try {
    sidecar = JSON.parse(fs.readFileSync(sidecarFile, 'utf8'))
  } catch (error) {
    return {
      group: 'desktop',
      name: 'desktop',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: `unreadable prebuild sidecar ${sidecarFile}: ${firstLine(error.message)}`
    }
  }
  const actual = crypto.createHash('sha256').update(fs.readFileSync(addon)).digest('hex')
  if (
    sidecar === null ||
    typeof sidecar !== 'object' ||
    sidecar.schema !== DESKTOP_SIDECAR_SCHEMA ||
    sidecar.sha256 !== actual
  ) {
    return {
      group: 'desktop',
      name: 'desktop',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: `prebuild ${addon} does not match its sidecar (substituted or corrupted)`
    }
  }
  let sealed
  try {
    sealed = JSON.parse(sidecar.identity)
  } catch {
    sealed = null
  }
  const staged = stagedDigests(sealed)
  if (staged === null) {
    return {
      group: 'desktop',
      name: 'desktop',
      state: 'stale',
      staged: null,
      current,
      refresh,
      detail: `${sidecarFile} carries no sealed sourceDigest/bindingSchema`
    }
  }
  if (staged.sourceDigest !== current.sourceDigest || staged.bindingSchema !== current.bindingSchema) {
    return {
      group: 'desktop',
      name: 'desktop',
      state: 'stale',
      staged,
      current,
      refresh,
      detail: `sourceDigest staged ${staged.sourceDigest}, sources ${current.sourceDigest}`
    }
  }
  return { group: 'desktop', name: 'desktop', state: 'fresh', staged, current, refresh, detail: null }
}

function classifyDesktop(root, identity, platform = process.platform, arch = process.arch) {
  const dir = `${platform}-${arch}`
  const results = [classifyDesktopDir(root, identity, dir)]
  for (const other of KNOWN_DESKTOP_DIRS) {
    if (other === dir) continue
    results.push({
      group: 'desktop',
      name: `desktop:${other}`,
      state: 'not-applicable',
      staged: null,
      current: null,
      refresh: null,
      detail: null,
      reason: `built by the CI matrix (pnpm native-prebuild:build); not built on ${dir}`
    })
  }
  return results
}

function classifyAll({ root, identity, platform = process.platform, arch = process.arch, only = GROUPS }) {
  for (const group of only) {
    if (!GROUPS.includes(group))
      throw new Error(`unknown native artifact group '${group}' (expected ${GROUPS.join(', ')})`)
  }
  const results = []
  if (only.includes('android')) results.push(classifyAndroid(root, identity))
  if (only.includes('apple')) results.push(classifyApple(root, identity, platform))
  if (only.includes('desktop')) results.push(...classifyDesktop(root, identity, platform, arch))
  return results
}

function formatDigests(label, digests) {
  if (digests === null) return `${label}=none`
  return `${label}=sourceDigest:${digests.sourceDigest} bindingSchema:${digests.bindingSchema}`
}

function formatResultLine(result) {
  if (result.state === 'not-applicable') return `${result.name}: not-applicable (${result.reason})`
  const line =
    `${result.name}: ${result.state} ` +
    `${formatDigests('staged', result.staged)} ${formatDigests('current', result.current)} ` +
    `refresh: ${result.refresh}`
  return result.detail === null || result.state === 'fresh' ? line : `${line} — ${result.detail}`
}

function runStatus({ root, identity, platform = process.platform, arch = process.arch, only = GROUPS }) {
  const results = classifyAll({ root, identity, platform, arch, only })
  const lines = results.map(formatResultLine)
  const failed = results.some(entry => entry.state === 'stale' || entry.state === 'missing')
  const json = JSON.stringify(
    results.map(entry => ({
      group: entry.group,
      name: entry.name,
      state: entry.state,
      staged: entry.staged,
      current: entry.current,
      refresh: entry.refresh,
      detail: entry.detail ?? null,
      reason: entry.reason ?? null
    }))
  )
  return { results, lines, json, exitCode: failed ? 1 : 0 }
}

function parseArguments(argv) {
  const options = { root: process.env.UBM_NATIVE_ROOT ?? ROOT, only: GROUPS, json: false }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--json') {
      options.json = true
      continue
    }
    if (argument === '--root' || argument === '--only') {
      const value = argv[index + 1]
      if (value === undefined || value.startsWith('--')) throw new Error(`${argument} needs a value`)
      options[argument.slice(2)] =
        argument === '--only'
          ? value
              .split(',')
              .map(entry => entry.trim())
              .filter(Boolean)
          : value
      index += 1
      continue
    }
    throw new Error(`unknown argument ${argument} (expected --root <dir>, --only android,apple,desktop, --json)`)
  }
  return options
}

if (require.main === module) {
  try {
    const options = parseArguments(process.argv.slice(2))
    const outcome = runStatus({ root: path.resolve(options.root), identity: resolveIdentity(), only: options.only })
    process.stdout.write(options.json ? `${outcome.json}\n` : `${outcome.lines.join('\n')}\n`)
    process.exitCode = outcome.exitCode
  } catch (error) {
    process.stderr.write(`native:status: ${firstLine(error.message)}\n`)
    process.exitCode = 2
  }
}

module.exports = {
  GROUPS,
  KNOWN_DESKTOP_DIRS,
  defaultRefreshCommand,
  classifyAndroid,
  classifyApple,
  classifyDesktop,
  classifyAll,
  formatResultLine,
  runStatus
}
