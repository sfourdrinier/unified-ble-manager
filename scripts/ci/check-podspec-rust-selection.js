#!/usr/bin/env node
// scripts/ci/check-podspec-rust-selection.js
//
// F01 iOS lane: the 5.x podspec selects the shared Rust core (UniFFI
// staticlib XCFramework built from the shipped bindings/uniffi sources +
// the generated Swift) alongside the Owned CoreBluetooth radio, which stays
// the thin platform adapter. 4.x keeps the Owned-only selection.
//
// Static, fast, Linux-runnable: parses the podspec selection branches and
// the build inputs. Compiling for the Apple matrix is
// `sh ios/build-rust-core.sh --check` (cargo check per target, no SDK) and
// runs in the F01 packed proof, not here.

'use strict'

const { spawnSync } = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')

const argv = process.argv.slice(2)
const rootFlag = argv.indexOf('--root')
// Default: this checkout. The F01 packed proof passes the installed
// consumer package root to evaluate the SHIPPED selection, not this tree.
const repoRoot =
  rootFlag === -1 ? path.resolve(__dirname, '..', '..') : path.resolve(argv[rootFlag + 1])

function fail(message) {
  throw new Error(`podspec-rust-selection FAIL: ${message}`)
}

function readRepoFile(relativePath) {
  try {
    return fs.readFileSync(path.join(repoRoot, relativePath), 'utf8')
  } catch (error) {
    fail(`missing ${relativePath}: ${error && error.message}`)
    return ''
  }
}

function evalPodspecOutcome(dir, extraEnv) {
  // The harness ships with this check (scripts/ never packs); only the
  // evaluated podspec/package come from the target root. extraEnv selects
  // the build mode under test (UBM_NATIVE_BUILD).
  const harness = path.join(__dirname, 'podspec-stub-eval.rb')
  const probed = spawnSync('ruby', [harness, '--dir', dir], {
    encoding: 'utf8',
    // Callers pass a complete env (so a mode can scrub a key); default is
    // ambient. Never merge here: spreading process.env first would
    // resurrect a deliberately deleted key.
    env: extraEnv === undefined ? process.env : { ...extraEnv }
  })
  if (probed.error) {
    fail(`podspec stub eval could not run ruby in ${dir}: ${probed.error.message}`)
  }
  return { status: probed.status, stdout: probed.stdout || '', stderr: probed.stderr || '' }
}

function evalPodspec(dir, extraEnv) {
  const probed = evalPodspecOutcome(dir, extraEnv)
  if (probed.status !== 0) {
    fail(`podspec stub eval failed in ${dir}: ${probed.stderr.trim()}`)
  }
  try {
    return JSON.parse(probed.stdout)
  } catch (error) {
    fail(`podspec stub eval printed no JSON in ${dir}: ${error && error.message}`)
    return {}
  }
}

function checkPodspecSelection() {
  const pod = readRepoFile('unified-ble-manager.podspec')
  // The 5.x gate: version-selected, never a default both lanes share.
  if (!pod.includes('start_with?("5.")')) {
    fail('podspec must gate the Rust core selection on the 5.x lane version')
  }
  // PR210-19 (ADR D2.6/D2.7) build modes, executed (real package.json) once
  // per mode. Unset or empty means prebuilt; `prebuilt` and `source` are
  // explicit; anything else is a hard pod-install error. No mode relies on
  // prepare_command: CocoaPods skips it for :path pods, so source
  // preparation is the explicit `native:apple:prepare` package script.
  const modeEnv = value => {
    const env = { ...process.env }
    if (value === undefined) delete env.UBM_NATIVE_BUILD
    else env.UBM_NATIVE_BUILD = value
    return env
  }
  const lane = evalPodspec(repoRoot, modeEnv(undefined))
  const emptyLane = evalPodspec(repoRoot, modeEnv(''))
  const prebuiltLane = evalPodspec(repoRoot, modeEnv('prebuilt'))
  const sourceLane = evalPodspec(repoRoot, modeEnv('source'))
  for (const invalid of ['bogus', 'SOURCE', ' source']) {
    const refused = evalPodspecOutcome(repoRoot, modeEnv(invalid))
    if (refused.status === 0) {
      fail(`5.x podspec must refuse UBM_NATIVE_BUILD=${JSON.stringify(invalid)} instead of silently choosing a mode`)
    }
    if (!refused.stderr.includes("UBM_NATIVE_BUILD must be unset, 'prebuilt' or 'source'")) {
      fail(`5.x podspec refused UBM_NATIVE_BUILD=${JSON.stringify(invalid)} without the actionable mode error: ${refused.stderr.trim()}`)
    }
  }
  const verifyMarker = '${PODS_TARGET_SRCROOT}/ios/verify-rust-core.sh'
  const staleMarker = 'scripts/release/native-build-identity.js" --root "${PODS_TARGET_SRCROOT}" --check-apple'
  for (const [mode, evaluated, expectStaleCheck] of [
    ['unset', lane, false],
    ['empty', emptyLane, false],
    ['prebuilt', prebuiltLane, false],
    ['source', sourceLane, true]
  ]) {
    if (evaluated.prepare_command !== undefined && evaluated.prepare_command !== null) {
      fail(`5.x podspec must not use prepare_command (${mode} mode): it never runs for :path pods`)
    }
    const phase = evaluated.script_phase
    if (phase === null || typeof phase !== 'object') {
      fail(`5.x podspec ${mode} mode must verify the staged RustCore`)
    }
    if (phase.name !== 'Verify staged RustCore' || phase.execution_position !== 'before_compile') {
      fail(`5.x podspec ${mode} mode must verify before_compile`)
    }
    if (typeof phase.script !== 'string' || !phase.script.includes(verifyMarker)) {
      fail(`5.x podspec ${mode} mode verify phase must run ios/verify-rust-core.sh`)
    }
    if (phase.script.includes('grep -c')) {
      fail(`5.x podspec ${mode} mode must parse the XCFramework, not grep-count LibraryIdentifier lines`)
    }
    if (phase.script.includes(staleMarker) !== expectStaleCheck) {
      fail(`5.x podspec ${mode} mode ${expectStaleCheck ? 'must' : 'must not'} run the source staleness check (--check-apple)`)
    }
    if (!(evaluated.preserve_paths || []).includes('ios/verify-rust-core.sh')) {
      fail(`5.x podspec ${mode} mode must preserve ios/verify-rust-core.sh for the script phase`)
    }
  }
  if (!sourceLane.script_phase.script.includes('native:apple:prepare')) {
    fail('5.x podspec source-mode phase must print the native:apple:prepare rerun command')
  }
  const scripts = JSON.parse(readRepoFile('package.json')).scripts || {}
  if (scripts['native:apple:prepare'] !== 'sh ios/build-rust-core.sh') {
    fail('package.json must expose native:apple:prepare = sh ios/build-rust-core.sh (the direct source preparation step)')
  }
  // 5.x lane outcome, executed (real package.json): Rust core selected
  // beside the Owned radio (mode-independent selection below).
  const sources = lane.source_files || []
  for (const selected of [
    'bindings/uniffi/generated/swift/ubm_echo.swift',
    'ios/UnifiedBleProtocolControl.mm',
    'ios/Owned/OwnedCoreBluetoothProtocolRadio.swift'
  ]) {
    if (!sources.includes(selected)) {
      fail(`5.x podspec source_files is missing ${selected}`)
    }
  }
  if ((lane.vendored_frameworks || []).indexOf('ios/RustCore/RustCore.xcframework') === -1) {
    fail('5.x podspec must vendor ios/RustCore/RustCore.xcframework')
  }
  const preserved = lane.preserve_paths || []
  for (const selected of [
    'bindings/uniffi/generated/swift/ubm_echoFFI.h',
    'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap'
  ]) {
    if (!preserved.includes(selected)) {
      fail(`5.x podspec preserve_paths is missing ${selected}`)
    }
  }
  // ubm_echo.swift compiles in the POD target: SWIFT_INCLUDE_PATHS must
  // be on pod_target_xcconfig (s.xcconfig reaches only the consumer
  // target and left canImport(ubm_echoFFI) false at pod compile time).
  // The merge must preserve the branch assignments, not clobber them.
  const podXcconfig = lane.pod_target_xcconfig
  if (
    podXcconfig === null ||
    typeof podXcconfig !== 'object' ||
    typeof podXcconfig.SWIFT_INCLUDE_PATHS !== 'string' ||
    !podXcconfig.SWIFT_INCLUDE_PATHS.includes('bindings/uniffi/generated/swift')
  ) {
    fail('5.x podspec must put the FFI modulemap on pod_target_xcconfig SWIFT_INCLUDE_PATHS')
  }
  for (const preserved of ['HEADER_SEARCH_PATHS', 'OTHER_CPLUSPLUSFLAGS', 'CLANG_CXX_LANGUAGE_STANDARD']) {
    if (typeof podXcconfig[preserved] !== 'string') {
      fail(`5.x podspec must preserve pod_target_xcconfig ${preserved} when merging SWIFT_INCLUDE_PATHS`)
    }
  }
  if (lane.xcconfig !== undefined && lane.xcconfig !== null) {
    fail('5.x podspec must not duplicate SWIFT_INCLUDE_PATHS onto consumer s.xcconfig')
  }
  // 4.x outcome, executed (stubbed package.json): Owned-only, no Rust.
  const legacyDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-podspec-4x-'))
  try {
    fs.copyFileSync(
      path.join(repoRoot, 'unified-ble-manager.podspec'),
      path.join(legacyDir, 'unified-ble-manager.podspec')
    )
    fs.writeFileSync(
      path.join(legacyDir, 'package.json'),
      JSON.stringify({
        version: '4.0.28',
        description: 'stub',
        homepage: 'https://example.invalid',
        license: 'SEE LICENSE',
        author: 'stub'
      })
    )
    const legacyEnv = { ...process.env }
    delete legacyEnv.UBM_NATIVE_BUILD
    const legacy = evalPodspec(legacyDir, legacyEnv)
    if (legacy.prepare_command !== undefined && legacy.prepare_command !== null) {
      fail('4.x podspec must not build the Rust core')
    }
    if ((legacy.source_files || []).includes('bindings/uniffi/generated/swift/ubm_echo.swift')) {
      fail('4.x podspec must not select the generated Swift')
    }
    if (!((legacy.source_files || []).includes('ios/Owned/OwnedCoreBluetoothProtocolRadio.swift'))) {
      fail('4.x podspec must keep the Owned radio')
    }
  } finally {
    fs.rmSync(legacyDir, { recursive: true, force: true })
  }
  console.log('podspec-rust-selection: modes unset/empty/prebuilt/source evaluated, invalid values refused')
  console.log('podspec-rust-selection: 5.x selects Rust+Owned, 4.x keeps Owned-only')
}

function checkBuildScript() {
  const scriptRelative = 'ios/build-rust-core.sh'
  const scriptPath = path.join(repoRoot, scriptRelative)
  if (!fs.existsSync(scriptPath)) {
    fail(`missing ${scriptRelative}`)
  }
  try {
    fs.accessSync(scriptPath, fs.constants.X_OK)
  } catch {
    fail(`${scriptRelative} is not executable`)
  }
  const probed = spawnSync('sh', ['-n', scriptPath], { encoding: 'utf8' })
  if (probed.status !== 0) {
    fail(`${scriptRelative} fails sh -n: ${(probed.stderr || '').trim()}`)
  }
  const script = fs.readFileSync(scriptPath, 'utf8')
  for (const marker of [
    'aarch64-apple-ios',
    'aarch64-apple-ios-sim',
    'create-xcframework',
    '--check',
    '--write-apple-identity',
    'verify-rust-core.sh'
  ]) {
    if (!script.includes(marker)) {
      fail(`${scriptRelative} is missing matrix piece ${marker}`)
    }
  }
  console.log('podspec-rust-selection: build script executable, syntax-clean, matrix-complete')
}

function checkRustInputs() {
  if (!fs.existsSync(path.join(repoRoot, 'bindings/uniffi/generated/swift/ubm_echo.swift'))) {
    fail('missing bindings/uniffi/generated/swift/ubm_echo.swift')
  }
  const manifest = readRepoFile('bindings/uniffi/Cargo.toml')
  if (!manifest.includes('"staticlib"')) {
    fail('bindings/uniffi/Cargo.toml crate-type must include "staticlib" for the iOS link')
  }
  console.log('podspec-rust-selection: generated Swift + staticlib crate-type present')
}

function main() {
  checkPodspecSelection()
  checkBuildScript()
  checkRustInputs()
  console.log('podspec-rust-selection PASS')
}

main()
