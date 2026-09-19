'use strict'

const path = require('path')

const NODE_API_VERSION = 8

/**
 * One maintained prebuild. `builder` names the toolchain: node-gyp for the
 * legacy C++/Objective-C addons (native/electron/<backend>), cargo for the
 * shared desktop Rust core (native/desktop-core, PR210-03). Cargo targets
 * carry the Rust target triple the runner builds.
 */
function target({ backend, platform, arch, runner, addonName, builder = 'node-gyp', moduleDirectory, rustTarget = null }) {
  const directory = moduleDirectory ?? path.posix.join('native', 'electron', backend)
  const prebuildPath = path.posix.join(directory, 'prebuilds', `${platform}-${arch}`, `${addonName}.node`)
  return Object.freeze({
    backend,
    platform,
    arch,
    runner,
    addonName,
    builder,
    rustTarget,
    moduleDirectory: directory,
    prebuildPath,
    // Cargo prebuilds ship an identity sidecar beside the binary: the file's
    // sha256 plus the binary's own nativeBuildIdentity() record.
    sidecarPath:
      builder === 'cargo' ? path.posix.join(path.posix.dirname(prebuildPath), `${addonName}.identity.json`) : null,
    artifactName: `native-prebuild-${backend}-${platform}-${arch}`
  })
}

function desktopCore(platform, arch, runner, rustTarget) {
  return target({
    backend: 'desktop-core',
    platform,
    arch,
    runner,
    addonName: 'ubm_desktop_core',
    builder: 'cargo',
    moduleDirectory: 'native/desktop-core',
    rustTarget
  })
}

const NATIVE_PREBUILD_TARGETS = Object.freeze([
  // LEGACY (5.0): node-gyp addons for the TypeScript CoreBluetooth/WinRT
  // backends. No public entrypoint loads them; Phase 4 deletes these rows
  // together with native/electron/{corebluetooth,winrt}.
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
  }),
  // The shared desktop Rust core every desktop entrypoint loads. Linux legs
  // build on ubuntu-22.04 (glibc 2.35 floor; libdbus-1 at runtime).
  desktopCore('linux', 'x64', 'ubuntu-22.04', 'x86_64-unknown-linux-gnu'),
  desktopCore('linux', 'arm64', 'ubuntu-22.04-arm', 'aarch64-unknown-linux-gnu'),
  desktopCore('darwin', 'arm64', 'macos-15', 'aarch64-apple-darwin'),
  desktopCore('darwin', 'x64', 'macos-15-intel', 'x86_64-apple-darwin'),
  desktopCore('win32', 'x64', 'windows-2025', 'x86_64-pc-windows-msvc'),
  desktopCore('win32', 'arm64', 'windows-11-arm', 'aarch64-pc-windows-msvc')
])

const NATIVE_PREBUILD_BACKENDS = Object.freeze([...new Set(NATIVE_PREBUILD_TARGETS.map(entry => entry.backend))])

module.exports = Object.freeze({ NODE_API_VERSION, NATIVE_PREBUILD_BACKENDS, NATIVE_PREBUILD_TARGETS })
