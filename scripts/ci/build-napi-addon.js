'use strict'

// scripts/ci/build-napi-addon.js — builds the shared desktop Rust core N-API
// addon (`ubm5_napi_echo`: the UbmCentral dispatch over ubm-desktop) from a
// checkout. Never shipped: packed consumers load the verified prebuilds under
// native/desktop-core/prebuilds/ (PR210-03).
//
// Identity (PR210-18): the build is sealed with the digests
// `native-build-identity.js --write --print-env napi` computes from the
// current sources (UBM_BUILD_SOURCE_DIGEST / UBM_BUILD_BINDING_SCHEMA), and
// the generated TypeScript expectation is refreshed in the same step, so the
// binary's `nativeBuildIdentity()` always matches the package it came from.
//
// Usage:
//   node scripts/ci/build-napi-addon.js [--profile debug|release] [--target <triple>] [--out <file.node>]
//
//   default: debug build staged at bindings/napi/ubm_echo.<platform>-<arch>.node
//            (the checkout's source-mode addon; load it with an absolute
//            UBM_NAPI_ADDON).
//   --out:   copy the built cdylib to <file.node> and write the prebuild
//            identity sidecar (ubm_desktop_core.identity.json) beside it,
//            recording the file's sha256 and the binary's own identity.
//
// Every failure is actionable (non-zero exit + the exact missing piece).

const { execFileSync } = require('node:child_process')
const crypto = require('node:crypto')
const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.join(__dirname, '..', '..')
const NAPI_DIR = path.join(ROOT, 'bindings', 'napi')
const IDENTITY_SCRIPT = path.join(ROOT, 'scripts', 'release', 'native-build-identity.js')
const SIDECAR_SCHEMA = 'ubm-desktop-core-prebuild/1'
const SIDECAR_NAME = 'ubm_desktop_core.identity.json'

const TARGET_LIBRARIES = Object.freeze({
  'aarch64-apple-darwin': 'libubm5_napi_echo.dylib',
  'x86_64-apple-darwin': 'libubm5_napi_echo.dylib',
  'aarch64-unknown-linux-gnu': 'libubm5_napi_echo.so',
  'x86_64-unknown-linux-gnu': 'libubm5_napi_echo.so',
  'aarch64-pc-windows-msvc': 'ubm5_napi_echo.dll',
  'x86_64-pc-windows-msvc': 'ubm5_napi_echo.dll'
})

function fail(message) {
  console.error(`build-napi-addon: FAIL ${message}`)
  process.exit(1)
}

function parseArguments(argv) {
  const options = { profile: 'debug', target: null, out: null }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--') continue
    const value = argv[index + 1]
    if (argument === '--profile' || argument === '--target' || argument === '--out') {
      if (value === undefined || value.startsWith('--')) fail(`${argument} needs a value`)
      options[argument.slice(2)] = value
      index += 1
      continue
    }
    fail(`unknown argument ${argument}`)
  }
  if (options.profile !== 'debug' && options.profile !== 'release') {
    fail(`--profile must be debug or release, got ${options.profile}`)
  }
  if (options.target !== null && TARGET_LIBRARIES[options.target] === undefined) {
    fail(`--target must be one of ${Object.keys(TARGET_LIBRARIES).join(', ')}`)
  }
  return options
}

function pinnedToolchain() {
  // Pinned toolchain single-sourced from rust-toolchain.toml — never hardcode.
  const toolchainFile = path.join(ROOT, 'rust-toolchain.toml')
  if (!fs.existsSync(toolchainFile)) fail(`toolchain pin missing: ${toolchainFile}`)
  const pin = /^channel\s*=\s*"([^"]+)"/m.exec(fs.readFileSync(toolchainFile, 'utf8'))
  if (pin === null) fail(`could not parse channel from ${toolchainFile}`)
  return pin[1]
}

function hostLibrary() {
  if (process.platform === 'linux') return 'libubm5_napi_echo.so'
  if (process.platform === 'darwin') return 'libubm5_napi_echo.dylib'
  if (process.platform === 'win32') return 'ubm5_napi_echo.dll'
  return fail(`unsupported platform '${process.platform}' (linux, darwin, win32 only)`)
}

/** Refresh the generated identity and read the digests cargo must seal. */
function identityEnvironment() {
  let output
  try {
    output = execFileSync(process.execPath, [IDENTITY_SCRIPT, '--write', '--print-env', 'napi'], {
      cwd: ROOT,
      encoding: 'utf8'
    })
  } catch (error) {
    return fail(`native-build-identity --write --print-env napi failed: ${error.message}`)
  }
  const environment = {}
  for (const line of output.split(/\r?\n/u)) {
    const match = /^(UBM_BUILD_SOURCE_DIGEST|UBM_BUILD_BINDING_SCHEMA)=([0-9a-f]{64})$/u.exec(line.trim())
    if (match !== null) environment[match[1]] = match[2]
  }
  if (environment.UBM_BUILD_SOURCE_DIGEST === undefined || environment.UBM_BUILD_BINDING_SCHEMA === undefined) {
    fail(`native-build-identity printed no napi digests:\n${output}`)
  }
  return environment
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
}

/** Load the staged addon in a child process and read its own identity. */
function readBinaryIdentity(file) {
  const probe =
    'const addon = require(process.argv[1]);' +
    "if (typeof addon.nativeBuildIdentity !== 'function') throw new Error('no nativeBuildIdentity export');" +
    "if (typeof addon.UbmCentral?.open !== 'function' || typeof addon.UbmCentral?.openSynthetic !== 'function') throw new Error('no UbmCentral.open/openSynthetic');" +
    'process.stdout.write(addon.nativeBuildIdentity())'
  try {
    return execFileSync(process.execPath, ['-e', probe, file], { encoding: 'utf8' })
  } catch (error) {
    return fail(`the built addon does not load or lacks its identity export: ${error.message}`)
  }
}

// Stage the addon under a fresh inode: write a sibling temporary file, then
// rename it over the destination. Copying over an image that a running
// process has mapped invalidates its cached code signature on macOS and kills
// that process; a rename leaves the old image intact for its current users.
function installAddon(built, destination) {
  fs.mkdirSync(path.dirname(destination), { recursive: true })
  const staging = `${destination}.staging-${process.pid}`
  fs.copyFileSync(built, staging)
  try {
    fs.renameSync(staging, destination)
  } catch (error) {
    fs.rmSync(staging, { force: true })
    throw error
  }
}

function main() {
  const options = parseArguments(process.argv.slice(2))
  const toolchain = pinnedToolchain()
  const identity = identityEnvironment()
  const cargoArgs = ['run', toolchain, 'cargo', 'build', '-p', 'ubm5_napi_echo', '--locked']
  if (options.profile === 'release') cargoArgs.push('--release')
  if (options.target !== null) cargoArgs.push('--target', options.target)
  console.log(
    `build-napi-addon: platform=${process.platform} arch=${process.arch} toolchain=${toolchain} ` +
      `profile=${options.profile} target=${options.target ?? 'host'} sourceDigest=${identity.UBM_BUILD_SOURCE_DIGEST}`
  )
  try {
    execFileSync('rustup', cargoArgs, {
      cwd: ROOT,
      stdio: 'inherit',
      env: {
        ...process.env,
        ...identity,
        ...(options.profile === 'release' ? { CARGO_PROFILE_RELEASE_STRIP: 'symbols' } : {})
      }
    })
  } catch {
    fail(
      `cargo build -p ubm5_napi_echo failed on ${toolchain} (see output above). ` +
        'Linux needs libdbus-1-dev + pkg-config (btleplug/bluez); macOS/Windows need no sysdeps.'
    )
  }

  const library = options.target === null ? hostLibrary() : TARGET_LIBRARIES[options.target]
  const targetDir = path.join(ROOT, 'target', ...(options.target === null ? [] : [options.target]), options.profile)
  const built = path.join(targetDir, library)
  if (!fs.existsSync(built)) fail(`expected cdylib missing after a successful build: ${built}`)

  const destination =
    options.out === null
      ? path.join(NAPI_DIR, `ubm_echo.${process.platform}-${process.arch}.node`)
      : path.resolve(options.out)
  installAddon(built, destination)
  const binaryIdentity = readBinaryIdentity(destination)
  if (options.out !== null) {
    const sidecar = {
      schema: SIDECAR_SCHEMA,
      sha256: sha256(destination),
      bytes: fs.statSync(destination).size,
      identity: binaryIdentity
    }
    fs.writeFileSync(path.join(path.dirname(destination), SIDECAR_NAME), `${JSON.stringify(sidecar, null, 2)}\n`)
  }
  console.log(`build-napi-addon: OK ${destination} (${fs.statSync(destination).size} bytes) identity=${binaryIdentity}`)
}

if (require.main === module) {
  main()
}

module.exports = { installAddon }
