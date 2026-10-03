'use strict'

const path = require('path')

const NODE_API_VERSION = 8

/**
 * One maintained shared desktop Rust prebuild. Every target carries the
 * Rust target triple the runner builds and its sealed identity sidecar.
 */
function target({ backend, platform, arch, runner, addonName, builder = 'cargo', moduleDirectory, rustTarget = null }) {
  const directory = moduleDirectory ?? 'native/desktop-core'
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
  // The shared desktop Rust core every desktop entrypoint loads. Linux legs
  // build on ubuntu-22.04 (glibc 2.35 floor; libdbus-1 at runtime).
  desktopCore('linux', 'x64', 'ubuntu-22.04', 'x86_64-unknown-linux-gnu'),
  desktopCore('linux', 'arm64', 'ubuntu-22.04-arm', 'aarch64-unknown-linux-gnu'),
  desktopCore('darwin', 'arm64', 'macos-15', 'aarch64-apple-darwin'),
  desktopCore('win32', 'x64', 'windows-2025', 'x86_64-pc-windows-msvc'),
  desktopCore('win32', 'arm64', 'windows-11-arm', 'aarch64-pc-windows-msvc')
])

const NATIVE_PREBUILD_BACKENDS = Object.freeze([...new Set(NATIVE_PREBUILD_TARGETS.map(entry => entry.backend))])

module.exports = Object.freeze({ NODE_API_VERSION, NATIVE_PREBUILD_BACKENDS, NATIVE_PREBUILD_TARGETS })
