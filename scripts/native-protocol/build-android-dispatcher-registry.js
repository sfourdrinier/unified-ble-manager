// Small host-only C++ registry peer for the Android dispatcher JVM tests.
'use strict'
const childProcess = require('child_process')
const path = require('path')
const output = process.argv[2]
if (!output) throw new Error('Expected output path for the registry test peer')
const root = path.resolve(__dirname, '../..')
const result = childProcess.spawnSync(process.env.CXX || 'c++', [
  '-std=c++20', '-O0', '-I', 'native/protocol/include', '-I', 'native/protocol/generated',
  '-I', 'node_modules/react-native/ReactCommon/jsi',
  'native/protocol/tests/AndroidDispatcherRegistryHarness.cpp',
  'native/protocol/src/NativeProtocolV2Codec.cpp', 'native/protocol/src/NativeProtocolControlRuntime.cpp',
  'native/protocol/src/NativeProtocolV2Registry.cpp', 'native/protocol/src/OwnedBinaryPayloadStore.cpp',
  'native/protocol/tests/OwnedJsiBinaryTransportHostStub.cpp', '-o', output
], { cwd: root, stdio: 'inherit' })
if (result.error) throw result.error
if (result.status !== 0) throw new Error(`Native registry test peer build failed: ${String(result.status)}`)
