#!/usr/bin/env node
// scripts/ci/f01-packed-dispatch-proof.js
//
// F01 acceptance proof: from THIS checkout, pack the exact 5.0.0-rc
// candidate, install it into a clean consumer with no sibling UBM checkout,
// and prove that manager creation, scan/connect/subscribe, timeout, and
// disposal execute the candidate Rust runtime — not the old TypeScript
// manager, not a sibling tree.
//
// Legs (every leg fails loudly; no skips, no fallbacks):
//   A. pack the candidate tarball; record its sha256 (immutable identity).
//   B. install into a fresh consumer (npm --ignore-scripts); assert no
//      sibling checkout and no prebuilt dispatch beside it.
//   C. run the packaging consumer gate (§D) against the install.
//   D. build the dispatch addon FROM THE PACKED SOURCES with the pinned
//      toolchain; assert the build consumed the packed tree.
//   E. verify package/core/fingerprint identity at runtime (version,
//      contract revision, seal), not mere importability.
//   F. drive scan/connect/discover/read/write/subscribe/timeout/dispose
//      through the packed-built addon (the dispatch round-trip).
//   G. prove the old TS manager cannot satisfy the proof (no UbmCentral).
//   H. compile the Tauri plugin from the packed sources (first consumer
//      build leg; its ubm-core references pin the shared core).
//   I. assert the Android arm64/x86_64 prebuilts are real ELF objects for
//      the declared ABIs (device load runs on device CI).
//   J. check the Rust core for the Apple matrix from packed sources and
//      evaluate the shipped podspec selection per lane.
//
// R16 labelling: legs A-E and G-J are REAL (packed candidate,
// pinned-toolchain builds from packed sources, runtime identity,
// compile/ELF checks). Leg F runs the REAL packed addon but with STAGED
// radio inputs — see the hardware-boundary declaration in
// bindings/napi/js/dispatch_roundtrip.cjs, which prints the branch taken.
// Real-binding device counterpart: the R01FACTORY emulator leg (ordinary
// factory, installed native module, no injection).
//
// Runtime: ~10-15 minutes (fresh cargo builds). Keeps the consumer and
// target dir under a temp root it prints; pass --keep to retain them.

'use strict'

const { spawnSync } = require('child_process')
const crypto = require('crypto')
const fs = require('fs')
const os = require('os')
const path = require('path')

const repoRoot = path.resolve(__dirname, '..', '..')
const keep = process.argv.slice(2).includes('--keep')
const workRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-f01-proof-'))
const targetDir = path.join(workRoot, 'cargo-target')
const consumerDir = path.join(workRoot, 'consumer')
fs.mkdirSync(consumerDir, { recursive: true })
fs.mkdirSync(targetDir, { recursive: true })

function log(message) {
  console.log(`f01-proof: ${message}`)
}

function fail(message) {
  throw new Error(`f01-proof FAIL: ${message}`)
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    ...options
  })
  if (result.status !== 0) {
    fail(
      `${command} ${args.join(' ')} exited ${result.status}\n` +
        `--- stdout ---\n${(result.stdout || '').slice(-4000)}\n--- stderr ---\n${(result.stderr || '').slice(-4000)}`
    )
  }
  return (result.stdout || '').trim()
}

function pinnedToolchain() {
  const toolchainFile = fs.readFileSync(path.join(repoRoot, 'rust-toolchain.toml'), 'utf8')
  const match = /^channel *= *"([^"]+)" */m.exec(toolchainFile)
  if (!match) fail('cannot parse pinned channel from rust-toolchain.toml')
  return match[1]
}

// Leg A: pack the exact candidate.
function legPack() {
  log(`work root: ${workRoot}`)
  const tarballName = run('npm', ['pack', '--pack-destination', workRoot, '--loglevel=warn'], {
    cwd: repoRoot
  })
    .split('\n')
    .filter(Boolean)
    .pop()
  const tarball = path.join(workRoot, tarballName)
  if (!tarball.endsWith('unified-ble-manager-5.0.0-rc.10.tgz')) {
    fail(`packed ${tarballName}, not the 5.0.0-rc.10 candidate`)
  }
  const sha256 = crypto.createHash('sha256').update(fs.readFileSync(tarball)).digest('hex')
  log(`candidate: ${tarballName} sha256=${sha256}`)
  return { tarball, sha256 }
}

// Leg B: clean consumer, no sibling checkout, no prebuilt dispatch.
function legInstall(tarball) {
  run('npm', ['init', '--yes'], { cwd: consumerDir })
  run('npm', ['install', '--ignore-scripts', '--no-audit', '--no-fund', tarball], { cwd: consumerDir })
  const installed = path.join(consumerDir, 'node_modules', 'unified-ble-manager')
  if (!fs.existsSync(path.join(installed, 'package.json'))) fail('consumer install missing package')
  // No sibling checkout beside the consumer: the only UBM tree in play is
  // the installed candidate.
  for (const entry of fs.readdirSync(workRoot, { withFileTypes: true })) {
    if (
      entry.isDirectory() &&
      entry.name !== 'consumer' &&
      entry.name !== 'cargo-target' &&
      /unified-ble-manager|UBM_5/i.test(entry.name)
    ) {
      fail(`sibling UBM tree beside the consumer: ${entry.name}`)
    }
  }
  if (fs.readdirSync(consumerDir).some(entry => entry !== 'node_modules' && entry !== 'package.json' && entry !== 'package-lock.json')) {
    fail('consumer dir is not a clean install')
  }
  // No prebuilt dispatch rides along: the proof builds it (leg D) or fails.
  // (Declared Electron prebuilds under native/electron prebuilts/ are the
  // §C-sanctioned exception.)
  const stray = []
  const walk = dir => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name)
      const relative = path.relative(installed, full)
      if (entry.isDirectory()) {
        walk(full)
      } else if (
        entry.name.endsWith('.node') &&
        !/^native[/\\]electron[/\\][^/\\]+[/\\]prebuilds[/\\]/u.test(relative)
      ) {
        stray.push(relative)
      }
    }
  }
  walk(installed)
  if (stray.length > 0) fail(`packed candidate ships prebuilt dispatch: ${stray.join(', ')}`)
  log(`clean consumer at ${installed} (no siblings, no prebuilt .node)`)
  return installed
}

// Leg C: packaging consumer gate on the install.
function legConsumerGate(installed) {
  run(process.execPath, [
    path.join(repoRoot, 'scripts', 'ci', 'check-napi-artifact-packaging.js'),
    '--consumer',
    installed
  ])
  log('packaging consumer gate (§D) holds on the install')
}

// Leg D: build the dispatch addon from the packed sources.
function legBuildDispatch(installed, toolchain, rustc) {
  const manifest = path.join(installed, 'Cargo.toml')
  if (!fs.existsSync(manifest)) fail('packed candidate is missing the workspace Cargo.toml')
  const active = run(rustc, ['--version'])
  if (!active.includes(toolchain)) {
    fail(`active ${active} is not the pinned ${toolchain}`)
  }
  run(
    'rustup',
    ['run', toolchain, 'cargo', 'build', '--locked', '-p', 'ubm5_napi_echo', '--manifest-path', manifest],
    { cwd: installed, env: { ...process.env, RUSTC: rustc, CARGO_TARGET_DIR: targetDir } }
  )
  const hostCdylib =
    process.platform === 'darwin'
      ? 'libubm5_napi_echo.dylib'
      : process.platform === 'win32'
        ? 'ubm5_napi_echo.dll'
        : 'libubm5_napi_echo.so'
  const built = path.join(targetDir, 'debug', hostCdylib)
  if (!fs.existsSync(built)) fail('dispatch build produced no cdylib')
  // The build consumed the packed tree: its fingerprint references the
  // installed sources, not this checkout. (grep exits 1 on no match.)
  const fingerprints = path.join(targetDir, 'debug', '.fingerprint')
  const probe = spawnSync('grep', ['-rl', repoRoot, fingerprints], {
    cwd: targetDir,
    encoding: 'utf8'
  })
  if (probe.status === 0 && (probe.stdout || '').trim().length > 0) {
    fail(
      `dispatch build references this checkout: ${probe.stdout.trim().split('\n').slice(0, 3).join(', ')}`
    )
  }
  if (probe.status !== 0 && probe.status !== 1) {
    fail(`fingerprint grep failed: ${(probe.stderr || '').trim()}`)
  }
  const addon = path.join(workRoot, 'ubm_dispatch.packed.node')
  fs.copyFileSync(built, addon)
  log(`dispatch addon built from packed sources: ${addon}`)
  return addon
}

// Leg E: package/core/fingerprint identity at runtime.
function legIdentity(installed, addon) {
  const manifest = JSON.parse(fs.readFileSync(path.join(installed, 'package.json'), 'utf8'))
  if (manifest.version !== '5.0.0-rc.10') fail(`installed version ${manifest.version} is not the candidate`)
  const sealPath = path.join(installed, 'lib', 'ubm-build-fingerprint.json')
  if (!fs.existsSync(sealPath)) fail('installed candidate is missing the fingerprint seal')
  const seal = JSON.parse(fs.readFileSync(sealPath, 'utf8'))
  if (seal.package.version !== '5.0.0-rc.10') fail('seal package version is not the candidate')
  const runtime = JSON.parse(
    run(process.execPath, ['-e', `console.log(JSON.stringify({revision: require(${JSON.stringify(addon)}).echoRevision()}))`])
  )
  if (runtime.revision !== seal.contractRevision) {
    fail(`addon core ${runtime.revision} does not match seal ${seal.contractRevision}`)
  }
  log(`identity: package ${manifest.version} / core ${runtime.revision} / seal present`)
}

// Leg F: ops through the packed-built addon.
function legDispatch(addon) {
  run(process.execPath, [path.join(repoRoot, 'bindings', 'napi', 'js', 'dispatch_roundtrip.cjs')], {
    env: { ...process.env, UBM_NAPI_ADDON: addon }
  })
  log('scan/connect/discover/read/write/subscribe/timeout/dispose executed the packed Rust runtime')
}

// Leg G: the old TS manager cannot satisfy the proof.
function legOldManager(installed) {
  const probe = run(
    process.execPath,
    [
      '-e',
      `const m = require(${JSON.stringify(installed)});` +
        `if ('UbmCentral' in m) { console.log('LEAK'); process.exit(3) }` +
        `console.log('sealed')`
    ]
  )
  if (probe !== 'sealed') fail('packed TS manager exposes the dispatch class')
  log('old TS manager exposes no dispatch surface (silent substitution impossible)')
}

// Leg H: the Tauri plugin compiles from the packed sources.
function legTauriPlugin(installed, toolchain, rustc) {
  const pluginDir = path.join(installed, 'native', 'tauri')
  if (!fs.existsSync(path.join(pluginDir, 'Cargo.toml'))) fail('packed candidate lost native/tauri')
  // No --locked: the plugin lockfile is local-only by design (see the
  // crate). Resolution runs against crates.io like any first consumer.
  run('rustup', ['run', toolchain, 'cargo', 'check', '-p', 'tauri-plugin-unified-ble-manager'], {
    cwd: pluginDir,
    env: { ...process.env, RUSTC: rustc, CARGO_TARGET_DIR: path.join(targetDir, 'tauri') }
  })
  log('Tauri plugin compiles from packed sources (ubm-core references pin the shared core)')
}

// Leg I: Android prebuilts are real objects for the declared ABIs.
function legAndroid(installed) {
  const ELF_MACHINE = { 'arm64-v8a': 183, 'x86_64': 62 }
  for (const [abi, machine] of Object.entries(ELF_MACHINE)) {
    const so = path.join(installed, 'android', 'src', 'main', 'jniLibs', abi, 'libubm5_jni_echo.so')
    if (!fs.existsSync(so)) fail(`packed candidate is missing the ${abi} prebuilt`)
    const header = fs.readFileSync(so).subarray(0, 20)
    if (!(header[0] === 0x7f && header[1] === 0x45 && header[2] === 0x4c && header[3] === 0x46)) {
      fail(`${abi} prebuilt is not ELF`)
    }
    if (header.readUInt16LE(18) !== machine) fail(`${abi} prebuilt has the wrong ELF machine`)
  }
  // PR210-18: the committed identity is JSON (build-identity.json).
  const identity = JSON.parse(
    fs.readFileSync(path.join(installed, 'android', 'src', 'main', 'jniLibs', 'build-identity.json'), 'utf8')
  )
  const recordedAbis = Array.isArray(identity.abis) ? identity.abis.map(entry => entry.abi) : []
  for (const abi of Object.keys(ELF_MACHINE)) {
    if (!recordedAbis.includes(abi)) fail(`build-identity.json does not cover ${abi}`)
  }
  log('Android arm64-v8a + x86_64 prebuilts are real ELF objects (device load runs on device CI)')
}

// Leg J: Apple matrix check + shipped podspec selection.
function legApple(installed) {
  run('sh', [path.join(installed, 'ios', 'build-rust-core.sh'), '--check'], { cwd: installed })
  run(process.execPath, [
    path.join(repoRoot, 'scripts', 'ci', 'check-podspec-rust-selection.js'),
    '--root',
    installed
  ])
  log('Apple matrix compiles from packed sources; shipped podspec selects per lane (link+load on macOS CI)')
}

function main() {
  try {
    const toolchain = pinnedToolchain()
    const rustc = run('rustup', ['which', '--toolchain', toolchain, 'rustc'])
    log(`pinned toolchain: ${toolchain}`)
    const { tarball, sha256 } = legPack()
    const installed = legInstall(tarball)
    legConsumerGate(installed)
    const addon = legBuildDispatch(installed, toolchain, rustc)
    legIdentity(installed, addon)
    legDispatch(addon)
    legOldManager(installed)
    legTauriPlugin(installed, toolchain, rustc)
    legAndroid(installed)
    legApple(installed)
    log(`F01 PACKED-DISPATCH PROOF PASS (candidate sha256=${sha256})`)
  } catch (error) {
    console.error(error && error.message ? error.message : error)
    process.exitCode = 1
  } finally {
    if (!keep) {
      fs.rmSync(workRoot, { recursive: true, force: true })
    } else {
      log(`kept work root: ${workRoot}`)
    }
  }
}

main()
