// scripts/native-protocol/test-apple-native-protocol.js

'use strict'

const childProcess = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')

const root = path.resolve(__dirname, '../..')
const reactNativeRoot = path.dirname(require.resolve('react-native/package.json'))
const reactCommonRoot = path.join(reactNativeRoot, 'ReactCommon')

function run(command, args, options = {}) {
  const result = childProcess.spawnSync(command, args, {
    cwd: root,
    stdio: 'inherit',
    shell: false,
    ...options
  })
  if (result.error) {
    throw result.error
  }
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed with exit code ${String(result.status)}`)
  }
}

if (process.platform !== 'darwin') {
  throw new Error('Apple Native Protocol executable harness requires macOS and Xcode')
}

function pinnedRustToolchain() {
  const contents = fs.readFileSync(path.join(root, 'rust-toolchain.toml'), 'utf8')
  const match = contents.match(/^channel\s*=\s*"([^"]+)"/m)
  if (match === null) throw new Error('rust-toolchain.toml has no pinned channel')
  return match[1]
}

const rustToolchain = pinnedRustToolchain()
const rustc = childProcess
  .execFileSync('rustup', ['which', '--toolchain', rustToolchain, 'rustc'], {
    encoding: 'utf8'
  })
  .trim()
const rustEnvironment = { ...process.env, RUSTC: rustc }

function runCargo(args) {
  run('rustup', ['run', rustToolchain, 'cargo', ...args], { env: rustEnvironment })
}

const temporaryDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'unified-ble-apple-native-protocol-'))
const executable = path.join(temporaryDirectory, 'AppleCoreBluetoothScanParserHarness')
const provenanceExecutable = path.join(temporaryDirectory, 'AppleCoreBluetoothReadNotifyProvenanceHarness')
const ingressExecutable = path.join(temporaryDirectory, 'AppleNativeIngressOrdinalHarness')
const borrowerOwnerExecutable = path.join(temporaryDirectory, 'AppleCoreBluetoothBorrowerOwnerHarness')
const executionExecutable = path.join(temporaryDirectory, 'AppleNativeProtocolExecutionHarness')
const rustRadioAdapterExecutable = path.join(temporaryDirectory, 'AppleRustRadioAdapterHarness')
const continuationExecutable = path.join(temporaryDirectory, 'AppleContinuationStatusHarness')
const accessorySources = [
  'ios/AccessoryChoiceOwner.swift',
  'ios/AccessoryChoiceAdmission.swift',
  'ios/UnifiedBleAccessoryChooser.swift'
]
const accessoryExecutable = path.join(temporaryDirectory, 'AccessoryChoiceOwnerHarness')
function testAccessoryChooser() {
  run('xcrun', [
    'swiftc',
    ...accessorySources.slice(0, 2),
    'ios/__tests__/AccessoryChoiceOwnerHarness.swift',
    '-o',
    accessoryExecutable
  ])
  run(accessoryExecutable, [])
  for (const [sdk, target] of [
    ['iphoneos', 'arm64-apple-ios16.4'],
    ['iphonesimulator', 'arm64-apple-ios16.4-simulator'],
    ['appletvsimulator', 'arm64-apple-tvos16.4-simulator']
  ]) {
    const sdkPath = childProcess.execFileSync('xcrun', ['--sdk', sdk, '--show-sdk-path'], { encoding: 'utf8' }).trim()
    run('xcrun', ['--sdk', sdk, 'swiftc', '-typecheck', '-target', target, '-sdk', sdkPath, ...accessorySources])
  }
}
const uniffiSwiftDirectory = path.join(root, 'bindings/uniffi/generated/swift')

// The Apple Rust route harness links the REAL mobile host: the host-platform
// build of the UniFFI crate, located through cargo (CARGO_TARGET_DIR aware).
function cargoTargetDirectory() {
  const result = childProcess.spawnSync(
    'rustup',
    ['run', rustToolchain, 'cargo', 'metadata', '--format-version', '1', '--no-deps', '--locked'],
    {
      cwd: root,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
      env: rustEnvironment
    }
  )
  if (result.error) {
    throw result.error
  }
  if (result.status !== 0) {
    throw new Error(`cargo metadata failed with exit code ${String(result.status)}: ${result.stderr}`)
  }
  return JSON.parse(result.stdout).target_directory
}
const ownedRadioSources = [
  path.join(root, 'ios/AccessoryChoiceAdmission.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothCentralDelegate.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadio.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioDescriptors.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioOwner.swift')
]
const continuationSources = [
  path.join(uniffiSwiftDirectory, 'ubm_echo.swift'),
  ...ownedRadioSources,
  path.join(root, 'ios/UnifiedBleRustRadioAdapter.swift'),
  path.join(root, 'ios/UnifiedBleRustCoreSessions.swift')
]

function compileIosContinuationBootstrap() {
  const sdk = childProcess
    .execFileSync('xcrun', ['--sdk', 'iphonesimulator', '--show-sdk-path'], { encoding: 'utf8' })
    .trim()
  const target = `${process.arch === 'arm64' ? 'arm64' : 'x86_64'}-apple-ios15.1-simulator`
  const header = path.join(temporaryDirectory, 'BlePlx-Swift.h')
  run('xcrun', [
    '--sdk',
    'iphonesimulator',
    'swiftc',
    '-parse-as-library',
    '-target',
    target,
    '-sdk',
    sdk,
    '-module-name',
    'BlePlx',
    '-emit-module',
    '-emit-module-path',
    path.join(temporaryDirectory, 'BlePlx.swiftmodule'),
    '-emit-objc-header-path',
    header,
    '-Xcc',
    `-fmodule-map-file=${path.join(uniffiSwiftDirectory, 'ubm_echoFFI.modulemap')}`,
    '-I',
    uniffiSwiftDirectory,
    ...continuationSources
  ])
  if (!fs.readFileSync(header, 'utf8').includes('bootstrapNativeContinuation'))
    throw new Error('native launch selector is not exported to Objective-C')
  const object = path.join(temporaryDirectory, 'UnifiedBleContinuationBootstrap.o')
  run('xcrun', [
    '--sdk',
    'iphonesimulator',
    'clang++',
    '-target',
    target,
    '-isysroot',
    sdk,
    '-fobjc-arc',
    '-fmodules',
    '-I',
    temporaryDirectory,
    '-c',
    path.join(root, 'ios/UnifiedBleContinuationBootstrap.mm'),
    '-o',
    object
  ])
  const symbols = childProcess.execFileSync('xcrun', ['nm', object], { encoding: 'utf8' })
  if (!symbols.includes('OBJC_CLASS_$_UnifiedBleContinuationBootstrap'))
    throw new Error('launch observer compiled out: Swift header was not imported')
  console.log(
    '[test-apple-native-protocol] iOS launch observer compiled against the real generated Swift interface; no OS relaunch claim.'
  )
}

try {
  testAccessoryChooser()
  run(process.execPath, [path.join(root, 'scripts/native-protocol/test-native-protocol.js')])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    ...ownedRadioSources,
    path.join(root, 'ios/__tests__/AppleCoreBluetoothScanParserHarness.swift'),
    '-o',
    executable
  ])
  run(executable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    ...ownedRadioSources,
    path.join(root, 'ios/__tests__/AppleCoreBluetoothReadNotifyProvenanceHarness.swift'),
    '-o',
    provenanceExecutable
  ])
  run(provenanceExecutable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    ...ownedRadioSources,
    path.join(root, 'ios/__tests__/AppleCoreBluetoothBorrowerOwnerHarness.swift'),
    '-o',
    borrowerOwnerExecutable
  ])
  run(borrowerOwnerExecutable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'clang++',
    '-std=c++20',
    '-pthread',
    path.join(root, 'ios/NativeProtocol/UnifiedBleProtocolAppleIngressTests.cpp'),
    '-o',
    ingressExecutable
  ])
  run(ingressExecutable, [])
  runCargo(['build', '--locked', '-p', 'ubm5_uniffi_echo'])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    '-parse-as-library',
    '-Xcc',
    `-fmodule-map-file=${path.join(uniffiSwiftDirectory, 'ubm_echoFFI.modulemap')}`,
    '-I',
    uniffiSwiftDirectory,
    ...continuationSources,
    path.join(root, 'ios/__tests__/AppleRustRadioAdapterHarness.swift'),
    '-L',
    path.join(cargoTargetDirectory(), 'debug'),
    '-lubm5_uniffi_echo',
    '-framework',
    'Security',
    '-framework',
    'CoreBluetooth',
    '-framework',
    'SystemConfiguration',
    '-o',
    rustRadioAdapterExecutable
  ])
  run(rustRadioAdapterExecutable, [])
  compileIosContinuationBootstrap()
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    '-parse-as-library',
    '-Xcc',
    `-fmodule-map-file=${path.join(uniffiSwiftDirectory, 'ubm_echoFFI.modulemap')}`,
    '-I',
    uniffiSwiftDirectory,
    ...continuationSources,
    path.join(root, 'ios/__tests__/AppleContinuationStatusHarness.swift'),
    '-L',
    path.join(cargoTargetDirectory(), 'debug'),
    '-lubm5_uniffi_echo',
    '-framework',
    'Security',
    '-framework',
    'CoreBluetooth',
    '-framework',
    'SystemConfiguration',
    '-o',
    continuationExecutable
  ])
  run(continuationExecutable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'clang++',
    '-x',
    'objective-c++',
    '-std=c++20',
    '-fobjc-arc',
    '-pthread',
    '-I',
    root,
    '-I',
    path.join(reactCommonRoot, 'jsi'),
    '-I',
    path.join(reactCommonRoot, 'callinvoker'),
    '-I',
    reactCommonRoot,
    path.join(root, 'native/protocol/tests/AppleNativeProtocolExecutionHarness.mm'),
    path.join(reactCommonRoot, 'jsi/jsi/jsi.cpp'),
    path.join(reactCommonRoot, 'jsc/JSCRuntime.cpp'),
    path.join(root, 'ios/NativeProtocol/UnifiedBleProtocolAppleBinaryDelivery.mm'),
    path.join(root, 'native/protocol/src/NativeProtocolV2Codec.cpp'),
    path.join(root, 'native/protocol/src/NativeProtocolV2Registry.cpp'),
    path.join(root, 'native/protocol/src/NativeProtocolControlRuntime.cpp'),
    path.join(root, 'native/protocol/src/OwnedBinaryPayloadStore.cpp'),
    path.join(root, 'native/protocol/src/OwnedJsiBinaryTransport.cpp'),
    '-framework',
    'Foundation',
    '-framework',
    'CoreBluetooth',
    '-framework',
    'JavaScriptCore',
    '-Wl,-undefined,dynamic_lookup',
    '-o',
    executionExecutable
  ])
  // Teardown regressions are lifetime-sensitive. Exercise fresh attach/fatal
  // terminal/close/runtime destruction cycles repeatedly without weakening
  // JSC's dangling-wrapper assertion.
  for (let iteration = 0; iteration < 3; iteration += 1) {
    run(executionExecutable, [])
  }
  // The ordinary reference-driver lane runs on Linux, where this Swift test
  // is intentionally skipped. Keep its compiled app transport and actual
  // emitted-envelope validation in the existing Apple executable gate.
  run(process.execPath, [
    '--test',
    path.join(root, 'example-expo/src/driver/__tests__/native-continuation-apple.test.mjs')
  ])
  console.log(
    '[test-apple-native-protocol] C++ protocol tests, the Apple CoreBluetooth parser, the CoreBluetooth read/notify provenance harness, the Rust mobile host ↔ Swift radio adapter harness, the Apple continuation declare/status harness, and the Apple execution CallInvoker/JSI terminal harness passed. No physical BLE radio or peripheral behavior was exercised.'
  )
} catch (error) {
  console.error('[test-apple-native-protocol] Apple Native Protocol executable harness failed:', error)
  process.exitCode = 1
} finally {
  fs.rmSync(temporaryDirectory, { recursive: true, force: true })
}
