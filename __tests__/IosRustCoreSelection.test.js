// __tests__/IosRustCoreSelection.test.js
//
// F01 iOS lane: the 5.x podspec selects the shared Rust core. Runs the
// static selection check (fail-closed on any drift).

'use strict'

const { execFileSync } = require('child_process')
const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')

test('5.x podspec selects the Rust core beside the Owned radio', () => {
  const script = path.join(__dirname, '..', 'scripts', 'ci', 'check-podspec-rust-selection.js')
  const output = execFileSync(process.execPath, [script], { encoding: 'utf8' })
  expect(output).toContain('podspec-rust-selection PASS')
})

test('5.x podspec stub declares the loud-failure consumer contract (no silent source build, no legacy fallback)', () => {
  const podspec = fs.readFileSync(path.join(root, 'unified-ble-manager.podspec'), 'utf8')
  // Prebuilt consumption: the exact framework the builder assembles.
  expect(podspec).toContain("s.vendored_frameworks = ['ios/RustCore/RustCore.xcframework']")
  // D2 modes: prebuilt default, explicit source selection only, canonical
  // builder in source mode, verified staging in both modes.
  expect(podspec).toContain("ubm_native_source_build = ENV['UBM_NATIVE_BUILD'] == 'source'")
  expect(podspec).toContain("s.prepare_command = 'sh ios/build-rust-core.sh'")
  expect(podspec).toContain("'Verify staged RustCore'")
  // The generated UniFFI Swift joins the pod module (never hand-edited outputs).
  expect(podspec).toContain('bindings/uniffi/generated/swift/ubm_echo.swift')
})

// R02 Apple cutover: a well-formed XCFramework (slice count + digests pass)
// must still prove it carries the REAL UniFFI core session. These anchors
// are the exact open/invoke/close + real-Central + scan-slice symbols the
// generated Swift binding references; ios/build-rust-core.sh attests every
// one of them as DEFINED in each assembled archive, and --check keeps the
// anchors coherent with the generated surface on Linux.
const CORE_FFI_ANCHORS = Object.freeze([
  'ffi_ubm5_uniffi_echo_fn_constructor_echosession_new',
  'ffi_ubm5_uniffi_echo_fn_method_echosession_close',
  'ffi_ubm5_uniffi_echo_fn_method_echosession_central_status',
  'ffi_ubm5_uniffi_echo_fn_method_echosession_ble_scan_start'
])

test('Apple core builder attests the real session symbols in every assembled archive', () => {
  const builder = fs.readFileSync(path.join(root, 'ios', 'build-rust-core.sh'), 'utf8')
  for (const anchor of CORE_FFI_ANCHORS) {
    expect(builder).toContain(anchor)
  }
  expect(builder).toContain('attest_core_symbols')
  expect(builder).toContain('carries no defined core symbol')
})

test('generated UniFFI Swift surface references every attested core symbol', () => {
  const generated = fs.readFileSync(
    path.join(root, 'bindings', 'uniffi', 'generated', 'swift', 'ubm_echo.swift'),
    'utf8'
  )
  for (const anchor of CORE_FFI_ANCHORS) {
    expect(generated).toContain(anchor)
  }
})
