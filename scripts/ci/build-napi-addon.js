'use strict'

// scripts/ci/build-napi-addon.js — R03 NAPI dispatch addon builder (UBM 5.0).
//
// Builds the real `ubm5_napi_echo` cdylib (F01 UbmCentral dispatch over
// ubm-desktop) and stages it under bindings/napi with the platform-correct
// file name the jest converged-path tests require:
//
//   ubm_echo.<process.platform>-<process.arch>.node
//
// Single source of truth for the build+stage step: called by
// `pretest:package` (so a fresh checkout builds on demand — the addon is
// never a committed artifact), by bindings/napi/run_napi_roundtrip.sh, and
// by the CI package/publish jobs before jest. Node (not shell) so the
// platform tag matches the test's expectation EXACTLY on every OS and the
// step runs under cmd.exe/pwsh as well as POSIX shells.
//
// Every failure is actionable (non-zero exit + the exact missing piece).

const { execFileSync } = require('node:child_process')
const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.join(__dirname, '..', '..')
const NAPI_DIR = path.join(ROOT, 'bindings', 'napi')

function fail(message) {
  console.error(`build-napi-addon: FAIL ${message}`)
  process.exit(1)
}

function main() {
  // Pinned toolchain single-sourced from rust-toolchain.toml — never hardcode.
  const toolchainFile = path.join(ROOT, 'rust-toolchain.toml')
  if (!fs.existsSync(toolchainFile)) fail(`toolchain pin missing: ${toolchainFile}`)
  const pin = /^channel\s*=\s*"([^"]+)"/m.exec(fs.readFileSync(toolchainFile, 'utf8'))
  if (pin === null) fail(`could not parse channel from ${toolchainFile}`)
  const pinned = pin[1]

  const platform = process.platform
  const arch = process.arch
  let artifact = null
  if (platform === 'linux') artifact = 'libubm5_napi_echo.so'
  else if (platform === 'darwin') artifact = 'libubm5_napi_echo.dylib'
  else if (platform === 'win32') artifact = 'ubm5_napi_echo.dll'
  if (artifact === null) fail(`unsupported platform '${platform}' (linux, darwin, win32 only)`)
  if (arch !== 'x64' && arch !== 'arm64') fail(`unsupported arch '${arch}' (x64 and arm64 only)`)
  const staged = `ubm_echo.${platform}-${arch}.node`

  console.log(`build-napi-addon: platform=${platform} arch=${arch} toolchain=${pinned}`)
  try {
    execFileSync('rustup', ['run', pinned, 'cargo', 'build', '-p', 'ubm5_napi_echo', '--locked'], {
      cwd: ROOT,
      stdio: 'inherit'
    })
  } catch {
    fail(
      `cargo build -p ubm5_napi_echo failed on ${pinned} (see output above). ` +
        'Linux needs libdbus-1-dev + pkg-config (btleplug/bluez); ' +
        'macOS/Windows need no sysdeps.'
    )
  }

  const built = path.join(ROOT, 'target', 'debug', artifact)
  if (!fs.existsSync(built)) fail(`expected cdylib missing after a successful build: ${built}`)
  const destination = path.join(NAPI_DIR, staged)
  fs.copyFileSync(built, destination)
  console.log(`build-napi-addon: OK ${destination} (${fs.statSync(destination).size} bytes)`)
}

main()
