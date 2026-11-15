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
const scanRecoveryFixtureHome = path.join(temporaryDirectory, 'scan-recovery-home')
const scanRecoveryFixtureDirectory = path.join(scanRecoveryFixtureHome, 'Library', 'Application Support', 'ubm-continuation')
const scanRecoveryLifecycleDirectory = path.join(temporaryDirectory, 'scan-recovery-lifecycle')
const executable = path.join(temporaryDirectory, 'AppleCoreBluetoothScanParserHarness')
const provenanceExecutable = path.join(temporaryDirectory, 'AppleCoreBluetoothReadNotifyProvenanceHarness')
const ingressExecutable = path.join(temporaryDirectory, 'AppleNativeIngressOrdinalHarness')
const borrowerOwnerExecutable = path.join(temporaryDirectory, 'AppleCoreBluetoothBorrowerOwnerHarness')
const borrowerRoutesExecutable = path.join(temporaryDirectory, 'AppleCachedRadioMultiBorrowerHarness')
const executionExecutable = path.join(temporaryDirectory, 'AppleNativeProtocolExecutionHarness')
const rustRadioAdapterExecutable = path.join(temporaryDirectory, 'AppleRustRadioAdapterHarness')
const scanLossAdapterExecutable = path.join(temporaryDirectory, 'AppleScanLossAdapterHarness')
const orderedGroupBridgeExecutable = path.join(temporaryDirectory, 'AppleRustOrderedGroupBridgeHarness')
const groupCaptureOwnerExecutable = path.join(temporaryDirectory, 'AppleOrderedGroupCaptureOwnerHarness')
const groupBridgeCaptureExecutable = path.join(temporaryDirectory, 'AppleOrderedGroupBridgeCaptureHarness')
const orderedIngressExecutable = path.join(temporaryDirectory, 'AppleOrderedNotificationIngressHarness')
const graphQueueExecutable = path.join(temporaryDirectory, 'AppleGraphDeferredQueueHarness')
const orderedRadioExecutable = path.join(temporaryDirectory, 'AppleOrderedRadioCallbackHarness')
const orderedSourceFactoryExecutable = path.join(temporaryDirectory, 'AppleOrderedSourceFactoryHarness')
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
    const sources =
      sdk === 'appletvsimulator'
        ? accessorySources
        : [...accessorySources, 'ios/__tests__/AccessoryChoiceDescriptorHarness.swift']
    run('xcrun', ['--sdk', sdk, 'swiftc', '-typecheck', '-target', target, '-sdk', sdkPath, ...sources])
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
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioDiscovery.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioDescriptors.swift'),
  path.join(root, 'ios/Owned/OwnedCoreBluetoothProtocolRadioOwner.swift'),
  path.join(root, 'ios/Owned/OwnedGraphDeferredQueue.swift'),
  path.join(root, 'ios/Owned/OwnedOrderedNotificationIngress.swift')
]
const continuationSources = [
  path.join(uniffiSwiftDirectory, 'ubm_echo.swift'),
  ...ownedRadioSources,
  path.join(root, 'ios/Owned/AppleOrderedGroupCaptureOwner.swift'),
  path.join(root, 'ios/Owned/UnifiedBleRustOrderedGroupBridge.swift'),
  path.join(root, 'ios/UnifiedBleRustRadioAdapter.swift'),
  path.join(root, 'ios/UnifiedBleRustOrderedRadioAdapter.swift'),
  path.join(root, 'ios/UnifiedBleRustCoreSessions.swift')
]

// A pod consumer imports the compiled Swift module, not the generated FFI
// source. Verify that the selected XCFramework-style Headers directory makes
// its transitive Clang module discoverable without application-supplied
// OTHER_SWIFT_FLAGS. UniFFI's generated map is custom-named, so the canonical
// Apple builder stages a derived conventional module.modulemap beside it.
function testSwiftConsumerFfiModuleDiscovery() {
  const headers = path.join(temporaryDirectory, 'ConsumerFfiHeaders')
  // Swift resolves `import BlePlx` from the module-name-derived filename.
  const module = path.join(temporaryDirectory, 'BlePlx.swiftmodule')
  const consumer = path.join(temporaryDirectory, 'ConsumerImportsBlePlx.swift')
  fs.mkdirSync(headers, { recursive: true })
  fs.copyFileSync(path.join(uniffiSwiftDirectory, 'ubm_echoFFI.h'), path.join(headers, 'ubm_echoFFI.h'))
  fs.copyFileSync(path.join(uniffiSwiftDirectory, 'ubm_echoFFI.modulemap'), path.join(headers, 'module.modulemap'))
  fs.writeFileSync(consumer, 'import BlePlx\n')
  run('xcrun', [
    '--sdk', 'macosx', 'swiftc', '-parse-as-library', '-emit-module', '-module-name', 'BlePlx',
    '-Xcc', `-I${headers}`, path.join(uniffiSwiftDirectory, 'ubm_echo.swift'),
    '-emit-module-path', module
  ])
  run('xcrun', [
    '--sdk', 'macosx', 'swiftc', '-typecheck', '-I', temporaryDirectory,
    '-Xcc', `-I${headers}`, consumer
  ])
}

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
  testSwiftConsumerFfiModuleDiscovery()
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
    '--sdk', 'macosx', 'swiftc',
    ...ownedRadioSources,
    path.join(root, 'ios/__tests__/AppleCachedRadioMultiBorrowerHarness.swift'),
    '-o', borrowerRoutesExecutable
  ])
  // Shared admission remains a red regression until physical ownership and
  // release are borrower-scoped. Exercise the implemented routing separately.
  for (const flag of [
    '--notification-cancel-stale-claim',
    '--ordered-native-claim-fence',
    '--notification-failure-cleanup-collision',
    '--notification-service-cleanup-collision',
    '--notification-join-stale-claim',
    '--notification-join-stale-route',
    '--notification-first-stale-join-current',
    '--notification-stale-disable',
    '--notification-stale-before-disconnect-fence',
    '--notification-teardown-replaced-claim',
    '--notification-teardown-replaced-route',
    '--notification-deferred-old-callback',
    '--notification-deferred-departed-error',
    '--notification-deferred-departed-legacy-error',
    '--notification-deferred-successful-disable',
    '--notification-full-gate-successful-disable',
    '--notification-immediate-failure-capture',
    '--notification-full-gate-error',
    '--notification-full-gate-off',
    '--notification-stale-claim-callback',
    '--notification-pending-claim',
    '--preparation-borrower-collision',
    '--stale-cleanup-operation-reuse',
    '--fallback-cleanup-receipt-collision',
    '--fallback-notification-collision',
    '--fallback-rssi-collision',
    '--fallback-connection-collision',
    '--scan-loss-isolation',
    '--scan-release-owner',
    '--scan-release-retry',
    '--scan-reset-fanout',
    '--connection-ownership-red',
    '--connection-cancel-collision',
    '--connection-pending-release',
    '--connection-cross-peer-collision',
    '--connection-failure-retry',
    '--connection-stale-callback',
    '--connection-terminal-fence',
    '--connection-final-release',
    '--claim-disconnect-routes',
    '--claim-disconnect-physical-cleanup',
    '--claim-partial-notification-cleanup',
    '--claim-partial-route-replaced',
    '--claim-notification-retention-refused',
    '--borrower-pending-failure-fence',
    '--claim-partial-other-borrower',
    '--claim-readiness-fanout',
    '--borrower-retry-nonconnection',
    '--claim-unresolved-cleanup-fence',
    '--claim-state-and-service-fanout',
    '--claim-shared-discovery',
    '--claim-discovery-retirement',
    '--claim-discovery-cancel-collision',
    '--claim-gatt-retirement',
    '--claim-gatt-cancel-collision',
    '--claim-gatt-generation',
    '--claim-gatt-write-fence',
    '--claim-gatt-cancel-write',
    '--claim-gatt-admission',
    '--claim-gatt-queued-cancel',
    '--claim-gatt-write-readiness',
    '--claim-descriptor-admission',
    '--claim-descriptor-retirement',
    '--claim-rssi-descriptor-cancel-collision',
    '--claim-descriptor-rssi-cancel-collision',
    '--claim-rssi-generation',
    '--claim-rssi-retirement',
    '--claim-discovery-receipt-collision',
    '--claim-gatt-write-limit'
  ]) run(borrowerRoutesExecutable, [flag])
  run(borrowerRoutesExecutable, ['--routes'])
  run(borrowerRoutesExecutable, ['--scan-duplicates'])
  run(borrowerRoutesExecutable, ['--scan-leases'])
  run(borrowerRoutesExecutable, ['--scan-fence'])
  run(borrowerRoutesExecutable, ['--scan-saturation'])
  run(borrowerRoutesExecutable, ['--scan-loss-lifecycle'])
  run(borrowerRoutesExecutable, ['--scan-teardown-race'])
  run(borrowerRoutesExecutable, ['--borrower-release-retry-isolation'])
  run(borrowerRoutesExecutable, ['--borrower-release-stale-retry'])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    path.join(root, 'ios/Owned/OwnedGraphDeferredQueue.swift'),
    path.join(root, 'ios/Owned/OwnedOrderedNotificationIngress.swift'),
    path.join(root, 'ios/__tests__/AppleOrderedNotificationIngressHarness.swift'),
    '-o',
    orderedIngressExecutable
  ])
  run(orderedIngressExecutable, [])
  run('xcrun', [
    '--sdk', 'macosx', 'swiftc',
    path.join(root, 'ios/Owned/OwnedGraphDeferredQueue.swift'),
    path.join(root, 'ios/__tests__/AppleGraphDeferredQueueHarness.swift'),
    '-o', graphQueueExecutable
  ])
  run(graphQueueExecutable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    ...ownedRadioSources,
    path.join(root, 'ios/__tests__/AppleOrderedRadioCallbackHarness.swift'),
    '-o',
    orderedRadioExecutable
  ])
  run(orderedRadioExecutable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    ...ownedRadioSources,
    path.join(root, 'ios/__tests__/AppleOrderedSourceFactoryHarness.swift'),
    '-o',
    orderedSourceFactoryExecutable
  ])
  run(orderedSourceFactoryExecutable, [])
  run('xcrun', [
    '--sdk',
    'macosx',
    'swiftc',
    path.join(root, 'ios/Owned/OwnedGraphDeferredQueue.swift'),
    path.join(root, 'ios/Owned/OwnedOrderedNotificationIngress.swift'),
    path.join(root, 'ios/Owned/AppleOrderedGroupCaptureOwner.swift'),
    path.join(root, 'ios/Owned/UnifiedBleRustOrderedGroupBridge.swift'),
    path.join(root, 'ios/__tests__/AppleRustOrderedGroupBridgeHarness.swift'),
    '-o',
    orderedGroupBridgeExecutable
  ])
  run(orderedGroupBridgeExecutable, [])
  run('xcrun', [
    '--sdk', 'macosx', 'swiftc',
    path.join(root, 'ios/Owned/OwnedGraphDeferredQueue.swift'),
    path.join(root, 'ios/Owned/OwnedOrderedNotificationIngress.swift'),
    path.join(root, 'ios/Owned/AppleOrderedGroupCaptureOwner.swift'),
    path.join(root, 'ios/Owned/UnifiedBleRustOrderedGroupBridge.swift'),
    path.join(root, 'ios/__tests__/AppleOrderedGroupCaptureOwnerHarness.swift'),
    '-o', groupCaptureOwnerExecutable
  ])
  run(groupCaptureOwnerExecutable, [])
  run('xcrun', [
    '--sdk', 'macosx', 'swiftc',
    path.join(root, 'ios/Owned/OwnedGraphDeferredQueue.swift'),
    path.join(root, 'ios/Owned/OwnedOrderedNotificationIngress.swift'),
    path.join(root, 'ios/Owned/AppleOrderedGroupCaptureOwner.swift'),
    path.join(root, 'ios/Owned/UnifiedBleRustOrderedGroupBridge.swift'),
    path.join(root, 'ios/__tests__/AppleOrderedGroupBridgeCaptureHarness.swift'),
    '-o', groupBridgeCaptureExecutable
  ])
  run(groupBridgeCaptureExecutable, [])
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
  // This isolated process uses a deterministic UniFFI authority only to force
  // lifecycle failure/race outcomes; it still crosses the production Sessions
  // and adapter boundaries.  A separate root prevents the process-global
  // recorder configuration from masking the recovery test.
  fs.mkdirSync(scanRecoveryLifecycleDirectory, { recursive: true })
  run(rustRadioAdapterExecutable, [], {
    env: {
      ...process.env,
      UBM_APPLE_SCAN_RECOVERY_DIRECTORY: scanRecoveryLifecycleDirectory,
      UBM_APPLE_SCAN_RECOVERY_LIFECYCLE_ONLY: '1'
    }
  })
  // The durable setup scenario must own its process-private recording root;
  // a second configuration in the main harness is correctly rejected.
  run(rustRadioAdapterExecutable, [], {
    env: { ...process.env, UBM_APPLE_DURABLE_SETUP_ONLY: '1' }
  })
  // Seed the durable recovery sidecars before the Swift process starts. The
  // harness gets only its normal application-support directory and exercises
  // the production native selection/UniFFI reopen path.
  fs.mkdirSync(scanRecoveryFixtureDirectory, { recursive: true })
  runCargo([
    'run', '--locked', '-p', 'ubm5_uniffi_echo', '--features', 'test-fixtures',
    '--bin', 'seed-apple-scan-recovery-fixture', '--', scanRecoveryFixtureDirectory
  ])
  run(rustRadioAdapterExecutable, [], {
    env: {
      ...process.env,
      HOME: scanRecoveryFixtureHome,
      UBM_APPLE_SCAN_RECOVERY_DIRECTORY: scanRecoveryFixtureDirectory,
      UBM_APPLE_SCAN_RECOVERY_ONLY: '1'
    }
  })
  run('xcrun', [
    '--sdk', 'macosx', 'swiftc', '-parse-as-library',
    '-Xcc', `-fmodule-map-file=${path.join(uniffiSwiftDirectory, 'ubm_echoFFI.modulemap')}`,
    '-I', uniffiSwiftDirectory,
    ...continuationSources,
    path.join(root, 'ios/__tests__/AppleScanLossAdapterHarness.swift'),
    '-L', path.join(cargoTargetDirectory(), 'debug'), '-lubm5_uniffi_echo',
    '-framework', 'Security', '-framework', 'CoreBluetooth', '-framework', 'SystemConfiguration',
    '-o', scanLossAdapterExecutable
  ])
  run(scanLossAdapterExecutable, [])
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
