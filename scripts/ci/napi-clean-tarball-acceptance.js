#!/usr/bin/env node
// scripts/ci/napi-clean-tarball-acceptance.js
//
// PR210-03 acceptance: a packed tarball, installed into a clean consumer
// with no Rust toolchain on PATH and no source overrides, loads THIS
// platform's prebuilt desktop core from the installed package, verifies its
// build identity, and executes the Rust core. It never packs anything
// itself: pass the tarball you want accepted.
//
//   node scripts/ci/napi-clean-tarball-acceptance.js --tarball <tgz>
//        [--pm npm|pnpm] [--probe identity|radio] [--negative] [--receipt <file>]
//
// Probes (both run as CommonJS and as ESM, from a cwd outside the consumer):
//   identity (default)  load the core through the installed package's own
//                       loader, verify identity, open + close one central on
//                       the SYNTHETIC radio (Rust executes; no Bluetooth
//                       permission needed; no radio opened). Safe in any
//                       process, including one macOS has not authorized for
//                       Bluetooth (TCC would SIGABRT a real radio open).
//   radio               the public no-options factory for this OS
//                       (createCoreBluetoothBleManager / createWinRtBleManager
//                       / createBluezBleManager): opens the real adapter.
//                       Pass: a manager whose backend is the Rust core, or
//                       adapter.unavailable on a headless host. Run it from a
//                       Bluetooth-authorized terminal on macOS.
//
// --negative legs (each on a fresh install), all through the PUBLIC factory
// and all required to fail BEFORE any radio open:
//   addon deleted                      -> no-prebuilt-for-target
//   addon truncated (sidecar kept)     -> prebuild-digest-mismatch (native-boundary.version)
//   addon corrupt (sidecar re-hashed)  -> load-failed
//   debug build, different identity    -> protocol.incompatible *.native-boundary.version
//   relative UBM_NAPI_ADDON            -> argument.invalid
//   legacy native/electron/*/index.js replaced by a throwing module
//                                      -> the default path is unaffected
//
// Emits a JSON receipt: { tarballSha256, pm, platform, arch, node,
// addonSha256, identity, outcome, dispatchCounters, negative }.

'use strict'

const { spawnSync } = require('node:child_process')
const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const repoRoot = path.resolve(__dirname, '..', '..')
const PLATFORMS = Object.freeze({
  linux: { platform: 'bluez', entry: 'node/bluez', factory: 'createBluezBleManager', prefix: 'bluez' },
  darwin: {
    platform: 'corebluetooth',
    entry: 'node/corebluetooth',
    factory: 'createCoreBluetoothBleManager',
    prefix: 'direct-gatt'
  },
  win32: { platform: 'winrt', entry: 'node/winrt', factory: 'createWinRtBleManager', prefix: 'winrt' }
})
const SOURCE_OVERRIDES = Object.freeze(['UBM_NAPI_ADDON', 'UBM_NATIVE_BUILD', 'UNIFIED_BLE_MANAGER_NATIVE_SOURCE'])

function fail(message) {
  throw new Error(`napi-clean-tarball-acceptance FAIL: ${message}`)
}

function parseArguments(argv) {
  const options = { pm: 'npm', probe: 'identity', negative: false, tarball: null, receipt: null }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    const value = argv[index + 1]
    if (argument === '--negative') {
      options.negative = true
      continue
    }
    if (['--tarball', '--pm', '--probe', '--receipt'].includes(argument)) {
      if (value === undefined) fail(`${argument} needs a value`)
      options[argument.slice(2)] = value
      index += 1
      continue
    }
    fail(`unknown argument ${argument}`)
  }
  if (options.tarball === null) fail('--tarball <tgz> is required (this script never packs)')
  if (!['npm', 'pnpm'].includes(options.pm)) fail('--pm must be npm or pnpm')
  if (!['identity', 'radio'].includes(options.probe)) fail('--probe must be identity or radio')
  return options
}

function sha256File(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
}

/** PATH without any Rust toolchain directory: the consumer must not build. */
function pathWithoutRust() {
  return (process.env.PATH ?? '')
    .split(path.delimiter)
    .filter(entry => !/[\\/](\.cargo|\.rustup)([\\/]|$)/u.test(entry))
    .filter(entry => !fs.existsSync(path.join(entry, process.platform === 'win32' ? 'cargo.exe' : 'cargo')))
    .join(path.delimiter)
}

function cleanEnvironment(extra = {}) {
  const environment = { ...process.env, PATH: pathWithoutRust() }
  for (const name of SOURCE_OVERRIDES) delete environment[name]
  return { ...environment, ...extra }
}

function run(command, args, options) {
  const result = spawnSync(command, args, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, ...options })
  if (result.error) fail(`${command} ${args.join(' ')}: ${result.error.message}`)
  return result
}

function install(tarball, pm) {
  const consumer = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-napi-acceptance-consumer-')))
  fs.writeFileSync(
    path.join(consumer, 'package.json'),
    JSON.stringify({ name: 'ubm-napi-acceptance-consumer', private: true, version: '0.0.0' }, null, 2)
  )
  const args =
    pm === 'npm'
      ? ['install', '--ignore-scripts', '--omit=optional', '--omit=peer', '--no-audit', '--no-fund', '--offline', tarball]
      : ['add', '--ignore-scripts', '--offline', tarball]
  const binary = process.platform === 'win32' ? `${pm}.cmd` : pm
  const result = run(binary, args, { cwd: consumer, env: cleanEnvironment(), shell: process.platform === 'win32' })
  if (result.status !== 0) {
    fail(`${pm} ${args.join(' ')} exited ${result.status} (offline install from the local cache)\n${result.stderr.slice(-3000)}`)
  }
  const packageRoot = path.join(consumer, 'node_modules', 'unified-ble-manager')
  if (!fs.existsSync(path.join(packageRoot, 'package.json'))) fail(`install produced no ${packageRoot}`)
  return { consumer, packageRoot }
}

function hostFacts() {
  const facts = PLATFORMS[process.platform]
  if (facts === undefined) fail(`no desktop platform for ${process.platform}`)
  return facts
}

/** The probe body, shared by the CJS and ESM probes (`load` differs). */
function probeBody(mode, facts) {
  return `
const facts = ${JSON.stringify(facts)};
const mode = ${JSON.stringify(mode)};
function summary(error) {
  // Public factories reject with the public BleError (code/domain/operation/platform
  // on the error itself); backend-level calls reject with a contract error (.normalized).
  const normalized = error && (error.normalized || (typeof error.code === 'string' && typeof error.domain === 'string' ? error : null));
  return normalized
    ? { code: normalized.code, domain: normalized.domain, operation: normalized.operation, platformCode: normalized.platform ? normalized.platform.code : null, detail: normalized.platform ? normalized.platform.safeMessage : null }
    : { code: 'uncaught', detail: String(error && error.stack || error) };
}
async function probe(load) {
  if (mode === 'identity') {
    const { loadDesktopCoreBinding } = await load('lib/desktop-core-addon.js');
    const binding = await loadDesktopCoreBinding({ platform: facts.platform, operationPrefix: facts.prefix });
    const central = await binding.openSynthetic('napi-acceptance');
    // One scan/connect/discover/subscribe/notify round trip on the synthetic
    // radio: every verb executes in Rust (the dispatch counters witness it).
    const HRM = '0000180d-0000-1000-8000-00805f9b34fb';
    const HRM_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb';
    const selector = { serviceUuid: HRM, serviceOccurrence: 0, characteristicUuid: HRM_MEASUREMENT, characteristicOccurrence: 0 };
    const pause = () => new Promise(resolve => setTimeout(resolve, 5));
    const scan = await central.startScan({ owner: 'napi-acceptance', serviceUuids: [HRM], timeoutMs: 5000 });
    await central.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Acceptance', serviceUuids: [HRM] });
    let seen = null;
    for (let attempt = 0; attempt < 400 && seen === null; attempt += 1) { seen = await central.takeScanObservation(); if (seen === null) await pause(); }
    await central.stopScan(scan.operationId);
    await central.connect({ peerId: 'peer-1', lease: 'lease-1', timeoutMs: 5000 });
    await central.stageServices('peer-1', [{ uuid: HRM, occurrence: 0, characteristics: [{ uuid: HRM_MEASUREMENT, occurrence: 0, properties: { read: true, write: false, writeWithoutResponse: false, notify: true, indicate: false }, descriptors: [] }] }]);
    await central.discover({ peerId: 'peer-1', lease: 'lease-1' });
    await central.subscribe({ peerId: 'peer-1', selector, consumer: 'app', timeoutMs: 5000 });
    await central.stageNotification({ peerId: 'peer-1', serviceUuid: HRM, characteristicUuid: HRM_MEASUREMENT, value: Buffer.from([6, 64]) });
    let value = null;
    for (let attempt = 0; attempt < 400 && value === null; attempt += 1) { const poll = await central.pollNotification({ peerId: 'peer-1', selector, consumer: 'app' }); if (poll.kind === 'value') value = poll.value; else await pause(); }
    await central.unsubscribe({ peerId: 'peer-1', selector, consumer: 'app' });
    await central.disconnect({ peerId: 'peer-1', lease: 'lease-1' });
    if (seen === null || value === null) throw new Error('synthetic round trip produced no advertisement or notification');
    const counters = central.dispatchCounters();
    const report = await central.close();
    return { outcome: report.state === 'released' ? 'identity-verified' : 'release-failed', diagnostics: binding.diagnostics, dispatchCounters: counters, close: report };
  }
  const entry = await load(facts.entry);
  let manager;
  try {
    manager = await entry[facts.factory]();
  } catch (error) {
    const failure = summary(error);
    return { outcome: failure.code === 'adapter.unavailable' ? 'adapter-unavailable' : 'failed', failure };
  }
  const state = await manager.adapter.state();
  await manager.destroy();
  return { outcome: 'manager-opened', adapter: state };
}
`
}

function writeProbes(consumer, packageRoot, mode) {
  const facts = hostFacts()
  const cjs = `'use strict'
const path = require('path');
${probeBody(mode, facts)}
const packageRoot = ${JSON.stringify(packageRoot)};
const load = async target =>
  target.startsWith('lib/') ? require(path.join(packageRoot, 'lib', 'commonjs', target.slice(4))) : require('unified-ble-manager/' + target);
probe(load).then(
  result => { console.log(JSON.stringify(result)); process.exit(0) },
  error => { console.log(JSON.stringify({ outcome: 'failed', failure: summary(error) })); process.exit(0) }
);
`
  const esm = `import { pathToFileURL } from 'node:url';
import path from 'node:path';
${probeBody(mode, facts)}
const packageRoot = ${JSON.stringify(packageRoot)};
const load = async target =>
  target.startsWith('lib/') ? import(pathToFileURL(path.join(packageRoot, 'lib', 'module', target.slice(4))).href) : import('unified-ble-manager/' + target);
probe(load).then(
  result => { console.log(JSON.stringify(result)); process.exit(0) },
  error => { console.log(JSON.stringify({ outcome: 'failed', failure: summary(error) })); process.exit(0) }
);
`
  fs.writeFileSync(path.join(consumer, 'probe.cjs'), cjs)
  fs.writeFileSync(path.join(consumer, 'probe.mjs'), esm)
}

function runProbe(consumer, file, extraEnvironment = {}) {
  const foreignCwd = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-napi-acceptance-cwd-'))
  const result = run(process.execPath, [path.join(consumer, file)], {
    cwd: foreignCwd,
    env: cleanEnvironment(extraEnvironment)
  })
  if (result.signal !== null) {
    fail(
      `${file} was killed by ${result.signal}${result.signal === 'SIGABRT' && process.platform === 'darwin' ? ' (macOS TCC: this process may not open Bluetooth; use --probe identity, or run --probe radio from a Bluetooth-authorized terminal)' : ''}`
    )
  }
  const line = result.stdout.trim().split('\n').filter(Boolean).pop()
  if (line === undefined) fail(`${file} printed nothing (exit ${result.status})\n${result.stderr.slice(-2000)}`)
  return JSON.parse(line)
}

function prebuildPaths(packageRoot) {
  const directory = path.join(packageRoot, 'native', 'desktop-core', 'prebuilds', `${process.platform}-${process.arch}`)
  return { addon: path.join(directory, 'ubm_desktop_core.node'), sidecar: path.join(directory, 'ubm_desktop_core.identity.json') }
}

function acceptPositive(outcome, mode, facts) {
  if (mode === 'identity') return outcome.outcome === 'identity-verified'
  if (outcome.outcome === 'adapter-unavailable') return true
  return outcome.outcome === 'manager-opened' && facts !== null
}

/** One negative leg on a fresh install: mutate, run the PUBLIC factory probe, check the refusal. */
function negativeLeg(tarball, pm, name, mutate, expected, environment = {}) {
  const { consumer, packageRoot } = install(tarball, pm)
  mutate(packageRoot)
  writeProbes(consumer, packageRoot, 'radio')
  const outcome = runProbe(consumer, 'probe.cjs', environment)
  const passed = expected(outcome)
  return { leg: name, passed, outcome }
}

function debugAddonForNegative() {
  const debug = path.join(repoRoot, 'bindings', 'napi', `ubm_echo.${process.platform}-${process.arch}.node`)
  if (!fs.existsSync(debug)) fail(`the debug-identity leg needs a checkout debug build at ${debug} (node scripts/ci/build-napi-addon.js)`)
  return debug
}

function main(argv) {
  const options = parseArguments(argv)
  const tarball = path.resolve(options.tarball)
  if (!fs.existsSync(tarball)) fail(`tarball not found: ${tarball}`)
  const facts = hostFacts()
  const tarballSha256 = sha256File(tarball)
  console.error(`napi-clean-tarball-acceptance: tarball ${tarball} sha256 ${tarballSha256}`)
  const { consumer, packageRoot } = install(tarball, options.pm)
  const { addon, sidecar } = prebuildPaths(packageRoot)
  if (!fs.existsSync(addon)) fail(`installed package has no prebuild for ${process.platform}-${process.arch}: ${addon}`)
  writeProbes(consumer, packageRoot, options.probe)
  const cjs = runProbe(consumer, 'probe.cjs')
  const esm = runProbe(consumer, 'probe.mjs')
  const receipt = {
    tarballSha256,
    pm: options.pm,
    platform: process.platform,
    arch: process.arch,
    node: process.version,
    probe: options.probe,
    addonSha256: sha256File(addon),
    identity: JSON.parse(JSON.parse(fs.readFileSync(sidecar, 'utf8')).identity),
    outcome: { cjs: cjs.outcome, esm: esm.outcome },
    dispatchCounters: cjs.dispatchCounters ?? null,
    details: { cjs, esm },
    negative: []
  }
  const positive =
    acceptPositive(cjs, options.probe, facts) &&
    acceptPositive(esm, options.probe, facts) &&
    (options.probe !== 'identity' ||
      ['scanStart', 'connect', 'discover', 'subscribe', 'notificationValues'].every(
        verb => (cjs.dispatchCounters?.[verb] ?? 0) > 0 && (esm.dispatchCounters?.[verb] ?? 0) > 0
      ))
  if (options.negative) {
    const failed = (code, platformCode) => outcome =>
      outcome.outcome === 'failed' &&
      outcome.failure.code === code &&
      (platformCode === null || outcome.failure.platformCode === platformCode)
    receipt.negative.push(
      negativeLeg(tarball, options.pm, 'addon deleted', root => fs.rmSync(prebuildPaths(root).addon), failed('capability.unavailable', 'no-prebuilt-for-target')),
      negativeLeg(
        tarball,
        options.pm,
        'addon truncated',
        root => fs.truncateSync(prebuildPaths(root).addon, 4096),
        failed('protocol.incompatible', 'prebuild-digest-mismatch')
      ),
      negativeLeg(
        tarball,
        options.pm,
        'addon corrupt, sidecar re-hashed',
        root => {
          const paths = prebuildPaths(root)
          fs.writeFileSync(paths.addon, Buffer.from('not a loadable image'))
          const record = JSON.parse(fs.readFileSync(paths.sidecar, 'utf8'))
          fs.writeFileSync(paths.sidecar, JSON.stringify({ ...record, sha256: sha256File(paths.addon) }))
        },
        failed('capability.unavailable', 'load-failed')
      ),
      negativeLeg(
        tarball,
        options.pm,
        'debug build with a different identity',
        root => {
          const paths = prebuildPaths(root)
          const debug = debugAddonForNegative()
          fs.copyFileSync(debug, paths.addon)
          const probe = run(process.execPath, ['-e', 'process.stdout.write(require(process.argv[1]).nativeBuildIdentity())', paths.addon], {})
          const record = JSON.parse(fs.readFileSync(paths.sidecar, 'utf8'))
          fs.writeFileSync(paths.sidecar, JSON.stringify({ ...record, sha256: sha256File(paths.addon), identity: probe.stdout }))
        },
        outcome =>
          outcome.outcome === 'failed' &&
          outcome.failure.code === 'protocol.incompatible' &&
          outcome.failure.operation === `${facts.prefix}.native-boundary.version`
      ),
      negativeLeg(tarball, options.pm, 'relative UBM_NAPI_ADDON', () => undefined, failed('argument.invalid', 'argument-invalid'), {
        UBM_NAPI_ADDON: 'relative/ubm_desktop_core.node'
      }),
      (() => {
        const { consumer: legacyConsumer, packageRoot: legacyRoot } = install(tarball, options.pm)
        for (const backend of ['corebluetooth', 'winrt']) {
          const legacy = path.join(legacyRoot, 'native', 'electron', backend, 'index.js')
          if (fs.existsSync(legacy)) fs.writeFileSync(legacy, "throw new Error('legacy loader must never be reached')\n")
        }
        writeProbes(legacyConsumer, legacyRoot, 'identity')
        const outcome = runProbe(legacyConsumer, 'probe.cjs')
        return { leg: 'legacy loaders poisoned', passed: outcome.outcome === 'identity-verified', outcome }
      })()
    )
  }
  const negativePassed = receipt.negative.every(leg => leg.passed)
  receipt.result = positive && negativePassed ? 'PASS' : 'FAIL'
  const text = `${JSON.stringify(receipt, null, 2)}\n`
  if (options.receipt !== null) fs.writeFileSync(options.receipt, text)
  process.stdout.write(text)
  if (receipt.result !== 'PASS') {
    process.exitCode = 1
  }
}

if (require.main === module) {
  try {
    main(process.argv.slice(2))
  } catch (error) {
    console.error(error && error.message ? error.message : error)
    process.exitCode = 1
  }
}

module.exports = { parseArguments, pathWithoutRust }
