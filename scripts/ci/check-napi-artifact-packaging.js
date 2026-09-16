#!/usr/bin/env node
// scripts/ci/check-napi-artifact-packaging.js
//
// UBM 5.0 PACKAGING slice (trackourhealth/bun-mono#1188, U8 vehicle).
// Candidate-only proof: the 5.0 candidate is identified by branch+SHA, never
// by a version bump, and nothing here publishes, tags, or merges.
//
// What this proves (fail-closed, offline unless a tarball/consumer is given):
//   A. every new 5.0 Rust crate carries publish=false + a resolvable SAL
//      license-file + repository pointer (crates/ubm-core, crates/ubm-desktop,
//      crates/ubm-fake-radio, bindings/napi, bindings/wasm, bindings/jni,
//      bindings/uniffi);
//   B. the npm export map ships none of the 5.0 dev-only surfaces (napi/wasm
//      artifacts, ubm-desktop consumers, TCK rust-driver, test-only fault
//      hooks, contracts) and the lane version makes no 5.0 release claim;
//   C. (--tarball) the packed tarball contains no bindings//crates trees, no
//      stray .node binaries, the license trio, and the same export guard on
//      the packed manifest;
//   D. (--consumer) an installed packed consumer cannot resolve the dev-only
//      subpaths, the shipped ./testing entry exposes no Rust/fault-hook
//      constructors, and the license trio is installed.
//
// Usage:
//   node scripts/ci/check-napi-artifact-packaging.js [--tarball <tgz>] [--consumer <dir>]
//
// The fast path (no flags) runs A+B only and needs no build.

'use strict'

const fs = require('fs')
const { createRequire } = require('module')
const path = require('path')

const repoRoot = path.resolve(__dirname, '..', '..')
const SAL_LICENSE_FILE = 'LICENSE-UBM-SOURCE-AVAILABLE-1.0.md'
const NOTICE_FILE = 'NOTICE'
const CONTRIBUTION_TERMS_FILE = 'UBM-CONTRIBUTION-TERMS-1.0.md'
const CANONICAL_REPOSITORY = 'https://github.com/sfourdrinier/unified-ble-manager'

// New 5.0 Rust crates: the npm-published JS surface never ships them, but
// their metadata must stay coherent so they can never leak onto crates.io
// with a wrong license.
const NEW_RUST_CRATES = Object.freeze([
  'crates/ubm-core',
  'crates/ubm-desktop',
  'crates/ubm-fake-radio',
  'bindings/napi',
  'bindings/wasm',
  'bindings/jni',
  'bindings/uniffi'
])

// 5.0 dev-only surfaces that must never become resolvable npm subpaths.
// contracts/ is a dev-time freeze, not runtime; napi/wasm are echo-only
// feasibility stand-ins (NOT BLE functionality); ubm-desktop is consumed via
// cargo, not npm; the TCK rust-driver needs the unpacked .node build; fault
// hooks are test infrastructure. See docs/5.0.0-PACKAGING.md.
const FORBIDDEN_EXPORT_SUBPATHS = Object.freeze([
  './napi',
  './wasm',
  './desktop',
  './tck/rust-driver',
  './test-only-fault-hooks',
  './contracts'
])

const FORBIDDEN_EXPORT_TARGET_FRAGMENTS = Object.freeze([
  'test-only-fault-hooks',
  'rust-driver',
  'bindings/',
  'crates/'
])

const FORBIDDEN_CONSUMER_SUBPATHS = Object.freeze([
  'unified-ble-manager/napi',
  'unified-ble-manager/wasm',
  'unified-ble-manager/desktop',
  'unified-ble-manager/tck/rust-driver',
  'unified-ble-manager/test-only-fault-hooks',
  'unified-ble-manager/contracts'
])

const FORBIDDEN_TESTING_RUNTIME_EXPORTS = Object.freeze([
  'RustBackendDriver',
  'createTestOnlyFaultHooks',
  'TCK_TEST_ONLY_MARKER',
  'assertTestOnlyFaultContext'
])

function fail(message) {
  throw new Error(`napi-artifact-packaging-proof FAIL: ${message}`)
}

function readRepoFile(relativePath) {
  return fs.readFileSync(path.join(repoRoot, relativePath), 'utf8').replace(/\r\n/g, '\n')
}

function manifestLine(content, field) {
  const match = content.match(new RegExp(`^\\s*${field}\\s*=\\s*"([^"]+)"\\s*(?:#.*)?$`, 'm'))
  return match ? match[1] : null
}

// Section A: Rust crate metadata coherence (offline, repo tree).
function checkRustCrateMetadata() {
  for (const crate of NEW_RUST_CRATES) {
    const manifestRelative = `${crate}/Cargo.toml`
    let content
    try {
      content = readRepoFile(manifestRelative)
    } catch (error) {
      fail(`missing Rust crate manifest ${manifestRelative}: ${error && error.message}`)
    }
    if (!/^\s*publish\s*=\s*false\s*(?:#.*)?$/m.test(content)) {
      fail(`${manifestRelative} must set publish = false (5.0 crates never publish to crates.io)`)
    }
    if (/^\s*license\s*=/m.test(content)) {
      fail(`${manifestRelative} must not use license = (new 5.0 material is SAL, not SPDX-short)`)
    }
    const licenseFile = manifestLine(content, 'license-file')
    if (licenseFile !== `../../${SAL_LICENSE_FILE}`) {
      fail(`${manifestRelative} license-file must equal "../../${SAL_LICENSE_FILE}", received ${String(licenseFile)}`)
    }
    if (!fs.existsSync(path.resolve(repoRoot, crate, licenseFile))) {
      fail(`${manifestRelative} license-file does not resolve: ${String(licenseFile)}`)
    }
    const repository = manifestLine(content, 'repository')
    if (repository !== CANONICAL_REPOSITORY) {
      fail(`${manifestRelative} repository must equal ${CANONICAL_REPOSITORY}, received ${String(repository)}`)
    }
    console.log(`napi-artifact-packaging-proof: ${manifestRelative} metadata coherent (publish=false + SAL license-file + repository)`)
  }
}

function checkExportMapGuard(exportsMap, label) {
  if (exportsMap === null || typeof exportsMap !== 'object' || Array.isArray(exportsMap)) {
    fail(`${label} exports must be an object`)
  }
  for (const subpath of FORBIDDEN_EXPORT_SUBPATHS) {
    if (Object.prototype.hasOwnProperty.call(exportsMap, subpath)) {
      fail(`${label} must not ship dev-only subpath ${subpath}`)
    }
  }
  const targets = []
  const collect = value => {
    if (typeof value === 'string') {
      targets.push(value)
      return
    }
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
      fail(`${label} export target must be a path or conditional object`)
    }
    for (const target of Object.values(value)) collect(target)
  }
  for (const [subpath, target] of Object.entries(exportsMap)) collect(target, subpath)
  for (const target of targets) {
    for (const fragment of FORBIDDEN_EXPORT_TARGET_FRAGMENTS) {
      if (target.includes(fragment)) {
        fail(`${label} export target must not reach dev-only material (${fragment}): ${target}`)
      }
    }
  }
}

// Section B: npm export-map dev-only guard + no-release-claim (offline).
function checkSourceExportMap() {
  const packageJson = JSON.parse(readRepoFile('package.json'))
  checkExportMapGuard(packageJson.exports, 'package.json')
  if (typeof packageJson.version !== 'string' || packageJson.version.length === 0) {
    fail('package.json version must be a non-empty string')
  }
  if (/^5\./.test(packageJson.version)) {
    fail(
      `package.json version ${packageJson.version} claims a 5.0 shipment; ` +
        'the 5.0 candidate is identified by branch+SHA, never by a version bump'
    )
  }
  for (const artifact of [SAL_LICENSE_FILE, NOTICE_FILE, CONTRIBUTION_TERMS_FILE]) {
    if (!Array.isArray(packageJson.files) || !packageJson.files.includes(artifact)) {
      fail(`package.json files must include ${artifact}`)
    }
  }
  console.log(
    `napi-artifact-packaging-proof: source export map clean ` +
      `(${Object.keys(packageJson.exports).length} subpaths, no dev-only surface; version ${packageJson.version} makes no 5.0 claim)`
  )
}

// Section C: packed tarball assertions.
function checkPackedTarball(tarballPath) {
  const { readTarball } = require('./verify-package-tarballs')
  const { NATIVE_PREBUILD_TARGETS } = require('../native-prebuilds/targets')
  const allowedPrebuilds = new Set(NATIVE_PREBUILD_TARGETS.map(target => `package/${target.prebuildPath}`))
  const files = readTarball(path.resolve(tarballPath))
  for (const entryPath of files.keys()) {
    if (entryPath === 'package/bindings' || entryPath.startsWith('package/bindings/')) {
      fail(`packed tarball must not contain N-API/WASM binding material: ${entryPath}`)
    }
    if (entryPath === 'package/crates' || entryPath.startsWith('package/crates/')) {
      fail(`packed tarball must not contain Rust crate sources: ${entryPath}`)
    }
    if (entryPath.endsWith('.node') && !allowedPrebuilds.has(entryPath)) {
      fail(`packed tarball contains a non-prebuild native binary: ${entryPath}`)
    }
  }
  for (const artifact of [SAL_LICENSE_FILE, NOTICE_FILE, CONTRIBUTION_TERMS_FILE, 'LICENSE']) {
    if (!files.has(`package/${artifact}`)) {
      fail(`packed tarball is missing ${artifact}`)
    }
  }
  const packedManifest = JSON.parse(files.get('package/package.json').toString('utf8'))
  checkExportMapGuard(packedManifest.exports, 'packed package.json')
  if (packedManifest.version !== JSON.parse(readRepoFile('package.json')).version) {
    fail(
      `packed version ${String(packedManifest.version)} drifts from source; ` +
        'packed tarballs must not relabel the candidate'
    )
  }
  console.log(
    `napi-artifact-packaging-proof: tarball clean (${files.size} entries, no bindings//crates trees, no stray .node, license trio present)`
  )
}

// Section D: installed packed consumer assertions.
function checkPackedConsumer(consumerDir) {
  const consumerRequire = createRequire(path.join(path.resolve(consumerDir), 'package.json'))
  for (const specifier of FORBIDDEN_CONSUMER_SUBPATHS) {
    try {
      const resolved = consumerRequire.resolve(specifier)
      fail(`packed consumer must not resolve ${specifier}, but resolved ${resolved}`)
    } catch (error) {
      if (!(error instanceof Error) || error.code !== 'ERR_PACKAGE_PATH_NOT_EXPORTED') {
        fail(`packed consumer resolution of ${specifier} failed unexpectedly: ${error && error.message}`)
      }
    }
  }
  console.log(
    `napi-artifact-packaging-proof: consumer subpath guard clean (${FORBIDDEN_CONSUMER_SUBPATHS.length} dev-only specifiers not exported)`
  )
  let testing
  try {
    testing = consumerRequire('unified-ble-manager/testing')
  } catch (error) {
    fail(`packed consumer cannot load unified-ble-manager/testing: ${error && error.message}`)
  }
  if (typeof testing !== 'object' || testing === null) {
    fail('packed unified-ble-manager/testing did not export an inspectable object')
  }
  for (const name of FORBIDDEN_TESTING_RUNTIME_EXPORTS) {
    if (name in testing) {
      fail(`packed unified-ble-manager/testing must not expose ${name}`)
    }
  }
  if (typeof testing.createDeterministicTestBackend !== 'function') {
    fail('packed unified-ble-manager/testing lost its public deterministic backend factory')
  }
  console.log('napi-artifact-packaging-proof: packed ./testing exposes no Rust/fault-hook constructors')
  const packageRoot = path.dirname(consumerRequire.resolve('unified-ble-manager/package.json'))
  for (const artifact of [SAL_LICENSE_FILE, NOTICE_FILE, CONTRIBUTION_TERMS_FILE, 'LICENSE']) {
    if (!fs.existsSync(path.join(packageRoot, artifact))) {
      fail(`installed packed consumer is missing ${artifact}`)
    }
  }
  for (const tree of ['bindings', 'crates']) {
    if (fs.existsSync(path.join(packageRoot, tree))) {
      fail(`installed packed consumer must not contain ${tree}/`)
    }
  }
  console.log('napi-artifact-packaging-proof: consumer license trio installed, no bindings//crates trees')
}

function runNapiArtifactPackagingProof(options = {}) {
  checkRustCrateMetadata()
  checkSourceExportMap()
  if (options.tarballPath !== undefined) checkPackedTarball(options.tarballPath)
  if (options.consumerDir !== undefined) checkPackedConsumer(options.consumerDir)
  console.log('napi-artifact-packaging-proof PASS')
  return true
}

function parseArguments(argv) {
  const options = {}
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--tarball') {
      const value = argv[index + 1]
      if (!value) fail('--tarball requires a path')
      options.tarballPath = value
      index += 1
      continue
    }
    if (argument === '--consumer') {
      const value = argv[index + 1]
      if (!value) fail('--consumer requires a directory')
      options.consumerDir = value
      index += 1
      continue
    }
    fail(`Unknown argument: ${argument}`)
  }
  return options
}

if (require.main === module) {
  try {
    runNapiArtifactPackagingProof(parseArguments(process.argv.slice(2)))
  } catch (error) {
    console.error(error && error.message ? error.message : error)
    process.exitCode = 1
  }
}

module.exports = { runNapiArtifactPackagingProof }
