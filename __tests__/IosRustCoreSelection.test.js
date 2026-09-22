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

test('5.x podspec declares the loud-failure consumer contract (explicit modes, no prepare_command, verified staging)', () => {
  const podspec = fs.readFileSync(path.join(root, 'unified-ble-manager.podspec'), 'utf8')
  // Prebuilt consumption: the exact framework the builder assembles.
  expect(podspec).toContain("s.vendored_frameworks = ['ios/RustCore/RustCore.xcframework']")
  // PR210-19: the mode is parsed and validated (unset/empty -> prebuilt,
  // prebuilt|source explicit, anything else raises). Previously any value
  // other than exactly `source` silently meant prebuilt.
  expect(podspec).toContain('raise Pod::Informative')
  expect(podspec).not.toContain("ENV['UBM_NATIVE_BUILD'] == 'source'")
  // PR210-19 (ADR D2.7): prepare_command never runs for :path pods, so it
  // is gone; source preparation is the explicit native:apple:prepare step.
  // Previously source mode set s.prepare_command = 'sh ios/build-rust-core.sh'.
  expect(podspec).not.toContain('prepare_command =')
  expect(podspec).toContain("'Verify staged RustCore'")
  // PR210-18: the phase parses and hash-verifies (ios/verify-rust-core.sh)
  // instead of grep-counting LibraryIdentifier lines.
  expect(podspec).toContain('ios/verify-rust-core.sh')
  expect(podspec).not.toContain('grep -c')
  // The generated UniFFI Swift joins the pod module (never hand-edited outputs).
  expect(podspec).toContain('bindings/uniffi/generated/swift/ubm_echo.swift')
})

test('CI stages ios/RustCore before an example installs the package copy that CocoaPods compiles', () => {
  // example-expo depends on `file:..`: pnpm copies the package's files into
  // its store at install time and Expo autolinking points the pod at that
  // copy, so a staging prepared after the install never reaches the pod.
  // (The classic example autolinks the repository root directly.)
  const apple = fs.readFileSync(path.join(root, '.github', 'workflows', 'apple-ci.yml'), 'utf8')
  const expoJob = apple.slice(apple.indexOf('  ios-expo:'), apple.indexOf('  tvos-library:'))
  const prepare = expoJob.indexOf('pnpm native:refresh --only apple')
  const install = expoJob.indexOf('pnpm --dir example-expo install')
  expect(prepare).toBeGreaterThan(-1)
  expect(install).toBeGreaterThan(prepare)
  const prepareStep = expoJob.slice(expoJob.lastIndexOf('- name:', prepare), prepare)
  expect(prepareStep).toContain('UBM_NATIVE_BUILD: source')
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

test('Apple core builder uses the exact pinned rustc and Cargo target directory', () => {
  const builder = fs.readFileSync(path.join(root, 'ios', 'build-rust-core.sh'), 'utf8')
  expect(builder).toContain('rustup which --toolchain "$PINNED_TOOLCHAIN" rustc')
  expect(builder).toContain('export RUSTC="$PINNED_RUSTC"')
  expect(builder).toContain('case "${CARGO_TARGET_DIR:-}" in')
  expect(builder).toContain('/*) CARGO_TARGET_ROOT="$CARGO_TARGET_DIR" ;;')
  expect(builder).toContain('*) CARGO_TARGET_ROOT="$ROOT/$CARGO_TARGET_DIR" ;;')
  expect(builder).toContain('BUILT_LIB="$CARGO_TARGET_ROOT/$1/$PROFILE_DIR/$LIB_NAME"')
  expect(builder).not.toContain('BUILT_LIB="$ROOT/target/$1/$PROFILE_DIR/$LIB_NAME"')
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
