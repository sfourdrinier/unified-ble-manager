'use strict'

const path = require('path')

const NODE_API_VERSION = 8

function target({ backend, platform, arch, runner, addonName }) {
  const moduleDirectory = path.posix.join('native', 'electron', backend)
  const prebuildPath = path.posix.join(moduleDirectory, 'prebuilds', `${platform}-${arch}`, `${addonName}.node`)
  return Object.freeze({
    backend,
    platform,
    arch,
    runner,
    addonName,
    moduleDirectory,
    prebuildPath,
    artifactName: `native-prebuild-${backend}-${platform}-${arch}`
  })
}

const NATIVE_PREBUILD_TARGETS = Object.freeze([
  target({
    backend: 'corebluetooth',
    platform: 'darwin',
    arch: 'arm64',
    runner: 'macos-15',
    addonName: 'unified_ble_corebluetooth'
  }),
  target({
    backend: 'corebluetooth',
    platform: 'darwin',
    arch: 'x64',
    runner: 'macos-15-intel',
    addonName: 'unified_ble_corebluetooth'
  }),
  target({
    backend: 'winrt',
    platform: 'win32',
    arch: 'arm64',
    runner: 'windows-11-arm',
    addonName: 'unified_ble_winrt'
  }),
  target({
    backend: 'winrt',
    platform: 'win32',
    arch: 'x64',
    runner: 'windows-2025',
    addonName: 'unified_ble_winrt'
  })
])

module.exports = Object.freeze({ NODE_API_VERSION, NATIVE_PREBUILD_TARGETS })
