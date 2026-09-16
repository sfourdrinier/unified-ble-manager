#!/usr/bin/env node
// scripts/release/generate-build-fingerprint.js
//
// F23: content fingerprint sealing the exact input set that produced `lib/`.
// Replaces the consumer max-mtime freshness heuristic (one touched output
// could mask a stale sibling): the seal records a sha256 per input file plus
// the package/contract/native identities, and the check fails on ANY drift —
// changed bytes, added files, removed files, tampered native binaries, or a
// tampered seal. Mtimes are never consulted, so touching an output cannot
// clear drift.
//
// Input set (fail-closed): the whole tree EXCEPT the denylist below. A
// denylist (not an allowlist) keeps new build inputs covered by default.
// Denylisted paths are never `lib/` inputs: build outputs, VCS/tooling
// state, test-only material, packed-but-unbuilt documents, and dev-only
// trees with their own builds.
//
// Identities sealed alongside the file map:
//   package          name@version that produced the build (version skew fails)
//   contractRevision C-UBM revision from crates/ubm-core/src/contracts.rs
//   native.android   committed jniLibs per ABI (file, sha256, bytes)
//   fingerprint      sha256 over the canonical seal (tamper-evident)
//
// Usage:
//   node scripts/release/generate-build-fingerprint.js [--check] [--root <dir>]
// Library: { generateBuildFingerprint, writeBuildFingerprint, checkBuildFingerprint }

'use strict'

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')

const SEAL_RELATIVE = path.join('lib', 'ubm-build-fingerprint.json')
const MAX_DRIFT_REPORT = 10

// Top-level trees that never feed `prepack` (own builds, records, or test
// fixtures). Everything else at top level is walked.
const EXCLUDED_TOP_DIRS = new Set([
  '__tests__', // jest suites import src/, never produce lib/
  'test-support', // imported only by __tests__
  'test_project', // scratch (gitignored)
  'docs', // prose + generated site, not lib/ inputs
  'etc', // api reports are reviewed records, not lib/ inputs
  'evidence', // gate evidence records
  'example', // separate package, own install
  'example-electron', // separate package, own install
  'example-expo', // separate package, own install
  'example-tauri', // separate package, own install
  'example-web', // separate package, own install
  'fixtures', // packed-consumer fixtures, not lib/ inputs
  'fuzz', // excluded cargo workspace member, own build
  'integration-tests', // post-build suites
  'lab', // scratch
  'emulator-probe' // post-build device probes
])

// Directory names excluded at ANY depth (outputs or tooling state).
const EXCLUDED_DIR_NAMES = new Set([
  'node_modules',
  'lib', // the build output itself (the seal lives here, never an input)
  'target', // cargo outputs
  'build', // gradle/tsc/cmake outputs (incl. plugin/build, android/build)
  'dist', // bundler outputs
  'coverage', // jest outputs
  'jvm-classes', // jni test outputs
  'Pods', // CocoaPods install output
  '.cxx', // AGP cmake outputs
  'prebuilds', // locally built electron prebuilds are CI-matrix outputs,
  // verified by native-prebuild:verify, never prepack inputs
  '__tests__' // suite trees at any depth
])

// Exact relative paths excluded (machine-local or consumer-regenerated).
const EXCLUDED_PATHS = new Set([
  'native/tauri/Cargo.lock', // library crate: gitignored, consumers' locks govern
  'android/local.properties' // machine-local SDK path
])

// Packed-but-unbuilt documents: they ride the tarball byte-for-byte at
// `pnpm pack` time, so editing one never stales `lib/`.
const EXCLUDED_FILE_NAMES = new Set([
  'SBOM.cdx.json',
  'THIRD_PARTY_LICENSES.json',
  'NOTICE',
  'LICENSE',
  'LICENSE-UBM-SOURCE-AVAILABLE-1.0.md',
  'UBM-CONTRIBUTION-TERMS-1.0.md',
  'llms.txt'
])

function isExcludedFile(relative) {
  const base = path.posix.basename(relative)
  if (base.startsWith('.')) return true // dotfiles: env/VCS state, never inputs
  if (relative.startsWith('.github/')) return true // CI workflows, not lib/ inputs
  if (EXCLUDED_PATHS.has(relative)) return true
  if (EXCLUDED_FILE_NAMES.has(base)) return true
  if (base.endsWith('.md')) return true // prose, never a build input
  if (base.endsWith('.tgz')) return true // packed outputs
  if (base.endsWith('.node')) return true // built native addons are outputs
  if (base.endsWith('.tsbuildinfo')) return true // tsc incremental state
  if (base.endsWith('.jsbundle')) return true // RN bundler outputs
  if (base.endsWith('.bat')) return true // generated wrapper scripts (gitignored)
  if (base.endsWith('.log')) return true // local logs, never inputs
  if (/\.test\.[cm]?[jt]s$/.test(base)) return true // test-only modules
  return false
}

function isExcludedDir(relative) {
  const segments = relative.split('/')
  if (segments.some(segment => segment.startsWith('.'))) return true // .git, .turbo, ...
  if (EXCLUDED_TOP_DIRS.has(segments[0])) return true
  return segments.some(segment => EXCLUDED_DIR_NAMES.has(segment))
}

function sha256File(absolute) {
  return crypto.createHash('sha256').update(fs.readFileSync(absolute)).digest('hex')
}

function walkInputs(root) {
  const files = {}
  const visit = relativeDir => {
    const absoluteDir = path.join(root, relativeDir)
    let entries
    try {
      entries = fs.readdirSync(absoluteDir, { withFileTypes: true })
    } catch (error) {
      if (relativeDir === '' && error && error.code === 'ENOENT') {
        throw new Error(`fingerprint root is missing: ${root}`)
      }
      throw error
    }
    for (const entry of entries) {
      const relative = relativeDir === '' ? entry.name : `${relativeDir}/${entry.name}`
      if (entry.isSymbolicLink()) continue // env links, never inputs
      if (entry.isDirectory()) {
        if (!isExcludedDir(relative)) visit(relative)
        continue
      }
      if (!entry.isFile() || isExcludedFile(relative)) continue
      files[relative] = sha256File(path.join(root, relative))
    }
  }
  visit('')
  return files
}

function readContractRevision(root) {
  const contracts = path.join(root, 'crates', 'ubm-core', 'src', 'contracts.rs')
  if (!fs.existsSync(contracts)) return null
  const match = fs
    .readFileSync(contracts, 'utf8')
    .match(/pub const CONTRACT_REVISION:\s*&str\s*=\s*"([^"]+)"/)
  return match ? match[1] : null
}

function androidNativeIdentity(root, files) {
  const prefix = 'android/src/main/jniLibs/'
  return Object.keys(files)
    .filter(relative => relative.startsWith(prefix) && relative.endsWith('.so'))
    .sort()
    .map(relative => ({
      abi: relative.slice(prefix.length).split('/')[0],
      file: relative,
      sha256: files[relative],
      bytes: fs.statSync(path.join(root, relative)).size
    }))
}

function canonicalSeal({ packageName, packageVersion, contractRevision, files, native }) {
  const sortedFiles = {}
  for (const relative of Object.keys(files).sort()) sortedFiles[relative] = files[relative]
  return { package: { name: packageName, version: packageVersion }, contractRevision, files: sortedFiles, native }
}

function sealDigest(canonical) {
  return crypto.createHash('sha256').update(JSON.stringify(canonical)).digest('hex')
}

function generateBuildFingerprint(root) {
  const absoluteRoot = path.resolve(root)
  const manifest = JSON.parse(fs.readFileSync(path.join(absoluteRoot, 'package.json'), 'utf8'))
  const files = walkInputs(absoluteRoot)
  if (Object.keys(files).length === 0) {
    throw new Error('fingerprint refuses an empty input set: refusing a vacuous seal')
  }
  const canonical = canonicalSeal({
    packageName: manifest.name,
    packageVersion: manifest.version,
    contractRevision: readContractRevision(absoluteRoot),
    files,
    native: { android: androidNativeIdentity(absoluteRoot, files) }
  })
  return { ...canonical, fingerprint: sealDigest(canonical) }
}

function sealPath(root) {
  return path.join(path.resolve(root), SEAL_RELATIVE)
}

function writeBuildFingerprint(root) {
  const seal = generateBuildFingerprint(root)
  const target = sealPath(root)
  fs.mkdirSync(path.dirname(target), { recursive: true })
  fs.writeFileSync(target, `${JSON.stringify(seal, null, 2)}\n`)
  return target
}

function driftReport(sealed, fresh) {
  const drifted = []
  for (const relative of Object.keys(sealed).sort()) {
    if (!(relative in fresh)) drifted.push(`${relative} (removed)`)
    else if (sealed[relative] !== fresh[relative]) drifted.push(relative)
  }
  for (const relative of Object.keys(fresh).sort()) {
    if (!(relative in sealed)) drifted.push(`${relative} (added)`)
  }
  return drifted
}

function checkBuildFingerprint(root) {
  const absoluteRoot = path.resolve(root)
  const target = sealPath(root)
  if (!fs.existsSync(target)) {
    throw new Error(
      `Build seal is missing: ${path.relative(absoluteRoot, target)}. Rebuild the library:\n  pnpm --dir ${absoluteRoot} prepack`
    )
  }
  let sealed
  try {
    sealed = JSON.parse(fs.readFileSync(target, 'utf8'))
  } catch {
    throw new Error(`Build seal is not valid JSON: ${target}. Rebuild the library:\n  pnpm --dir ${absoluteRoot} prepack`)
  }
  const { fingerprint, ...stored } = sealed
  if (typeof fingerprint !== 'string' || sealDigest(stored) !== fingerprint) {
    throw new Error(`Build seal integrity failed (tampered or truncated): ${target}. Rebuild the library:\n  pnpm --dir ${absoluteRoot} prepack`)
  }
  const manifest = JSON.parse(fs.readFileSync(path.join(absoluteRoot, 'package.json'), 'utf8'))
  if (stored.package === undefined || stored.package.version !== manifest.version) {
    throw new Error(
      `Build seal targets ${stored.package === undefined ? 'unknown' : stored.package.version}, but the checkout is ${manifest.version}. Rebuild the library:\n  pnpm --dir ${absoluteRoot} prepack`
    )
  }
  const fresh = generateBuildFingerprint(absoluteRoot)
  const drifted = driftReport(stored.files === undefined ? {} : stored.files, fresh.files)
  if (stored.contractRevision !== fresh.contractRevision) {
    drifted.unshift(`contract revision ${String(stored.contractRevision)} -> ${String(fresh.contractRevision)}`)
  }
  if (drifted.length > 0) {
    const shown = drifted.slice(0, MAX_DRIFT_REPORT).join('\n  ')
    const more = drifted.length > MAX_DRIFT_REPORT ? `\n  ... and ${drifted.length - MAX_DRIFT_REPORT} more` : ''
    throw new Error(
      `Build output is stale: ${drifted.length} input(s) changed since the seal was written:\n  ${shown}${more}\nRebuild the library:\n  pnpm --dir ${absoluteRoot} prepack`
    )
  }
  return true
}

function parseArguments(argv) {
  const options = { check: false, root: path.resolve(__dirname, '..', '..') }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--check') {
      options.check = true
      continue
    }
    if (argument === '--root') {
      const value = argv[index + 1]
      if (value === undefined) throw new Error('--root requires a directory')
      options.root = path.resolve(value)
      index += 1
      continue
    }
    throw new Error(`Unknown argument: ${argument}`)
  }
  return options
}

if (require.main === module) {
  try {
    const options = parseArguments(process.argv.slice(2))
    if (options.check) {
      checkBuildFingerprint(options.root)
      process.stdout.write('build-fingerprint: seal is current\n')
    } else {
      const seal = generateBuildFingerprint(options.root)
      const target = writeBuildFingerprint(options.root)
      process.stdout.write(
        `build-fingerprint: sealed ${Object.keys(seal.files).length} inputs -> ${path.relative(options.root, target)} (${seal.fingerprint.slice(0, 12)})\n`
      )
    }
  } catch (error) {
    process.stderr.write(`${error && error.message ? error.message : error}\n`)
    process.exitCode = 1
  }
}

module.exports = { SEAL_RELATIVE, generateBuildFingerprint, writeBuildFingerprint, checkBuildFingerprint }
