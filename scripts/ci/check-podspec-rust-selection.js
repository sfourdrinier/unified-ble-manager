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

function evalPodspec(dir) {
  // The harness ships with this check (scripts/ never packs); only the
  // evaluated podspec/package come from the target root.
  const harness = path.join(__dirname, 'podspec-stub-eval.rb')
  const probed = spawnSync('ruby', [harness, '--dir', dir], { encoding: 'utf8' })
  if (probed.status !== 0) {
    fail(`podspec stub eval failed in ${dir}: ${(probed.stderr || '').trim()}`)
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
  // 5.x lane outcome, executed (real package.json): Rust core selected
  // beside the Owned radio.
  const lane = evalPodspec(repoRoot)
  if (lane.prepare_command !== 'sh ios/build-rust-core.sh') {
    fail('5.x podspec must build the Rust core at pod install')
  }
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
  if (
    lane.xcconfig === null ||
    typeof lane.xcconfig !== 'object' ||
    typeof lane.xcconfig.SWIFT_INCLUDE_PATHS !== 'string' ||
    !lane.xcconfig.SWIFT_INCLUDE_PATHS.includes('bindings/uniffi/generated/swift')
  ) {
    fail('5.x podspec must put the FFI modulemap on SWIFT_INCLUDE_PATHS')
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
    const legacy = evalPodspec(legacyDir)
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
  for (const marker of ['aarch64-apple-ios', 'aarch64-apple-ios-sim', 'create-xcframework', '--check']) {
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
