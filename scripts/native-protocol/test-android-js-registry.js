// Run the historical Android JS command stream through the real JSI codec and registry.
'use strict'

const childProcess = require('child_process')
const fs = require('fs')
const os = require('os')
const path = require('path')

const root = path.resolve(__dirname, '../..')
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-android-js-registry-'))
const executable = path.join(temporary, process.platform === 'win32' ? 'registry.exe' : 'registry')

function run(command, args, env = process.env) {
  const result = childProcess.spawnSync(command, args, { cwd: root, env, stdio: 'inherit' })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`${command} exited with status ${String(result.status)}`)
}

try {
  run(process.env.CXX || 'c++', [
    '-std=c++20', '-O0',
    '-I', 'native/protocol/include', '-I', 'native/protocol/generated',
    '-I', 'node_modules/react-native/ReactCommon/jsi',
    'native/protocol/tests/AndroidJsRegistryBoundary.cpp',
    'native/protocol/src/NativeProtocolV2Codec.cpp',
    'native/protocol/src/NativeProtocolControlRuntime.cpp',
    'native/protocol/src/NativeProtocolV2Registry.cpp',
    'native/protocol/src/OwnedBinaryPayloadStore.cpp',
    'native/protocol/tests/OwnedJsiBinaryTransportHostStub.cpp',
    '-o', executable
  ])
  const config = path.join(temporary, 'jest.config.cjs')
  fs.writeFileSync(config, `module.exports = { ...require(${JSON.stringify(path.join(root, 'jest.config.js'))}), rootDir: ${JSON.stringify(root)} }\n`)
  run(process.execPath, [
    require.resolve('jest/bin/jest'), '--config', config, '--runInBand',
    '__tests__/backends/reactnative/react-native-android-vertical-slice.test.js'
  ], { ...process.env, UBM_ANDROID_REGISTRY_HELPER: executable })
} catch (error) {
  console.error(error)
  process.exitCode = 1
} finally {
  fs.rmSync(temporary, { recursive: true, force: true })
}
