'use strict'

// Packed desktop acceptance for Bun 1.4.2 and Node.
//
// Installs the packed tarball into a clean consumer, then checks CJS and ESM
// public entries, the six public manager scenarios, sealed prebuild identity,
// and a waker that actually fires. This script packs. Do not import it from a
// unit test that should stay hermetic: the suite reads the source.
//
//   node scripts/ci/bun-packed-desktop-acceptance.js

const { spawnSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const ROOT = path.join(__dirname, '..', '..')
const MINIMUM_BUN = [1, 4, 2]
const HRM = '0000180d-0000-1000-8000-00805f9b34fb'
const INSTALLED_IDENTITY = 'src/generated/native-build-identity.ts'

const MANAGER_SCENARIO_IDS = Object.freeze([
  'manager.scan-connect-discover-read-notify-destroy',
  'manager.cancellation-deadline-and-late-completion',
  'manager.overflow-late-events-and-stream-settlement',
  'manager.generation-invalidation-reconnect-and-rediscovery',
  'manager.two-client-arbitration-and-retryable-cleanup',
  'manager.adapter-loss-and-zero-counter-settlement'
])

function fail(message) {
  console.error(`bun-packed-desktop-acceptance: FAIL ${message}`)
  process.exit(1)
}

function run(command, args, options) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    ...options
  })
  if (result.error) fail(`${command} ${args.join(' ')}: ${result.error.message}`)
  if (result.status !== 0) {
    fail(
      `${command} ${args.join(' ')} exited ${result.status}\nstdout:\n${(result.stdout || '').slice(-4000)}\nstderr:\n${(result.stderr || '').slice(-4000)}`
    )
  }
  return result
}

function versionBelow(actual, minimum) {
  const parts = String(actual)
    .split('.')
    .map(part => Number.parseInt(part, 10))
  for (let index = 0; index < minimum.length; index += 1) {
    const left = parts[index] ?? 0
    const right = minimum[index]
    if (left < right) return true
    if (left > right) return false
  }
  return false
}

function bunBinary() {
  const candidates = [process.env.BUN_BIN, path.join(os.homedir(), '.bun', 'bin', 'bun'), 'bun'].filter(Boolean)
  for (const candidate of candidates) {
    const result = spawnSync(candidate, ['--version'], { encoding: 'utf8' })
    if (result.status === 0) {
      const version = String(result.stdout || '').trim()
      if (versionBelow(version, MINIMUM_BUN)) fail(`Bun ${version} is older than ${MINIMUM_BUN.join('.')}`)
      return candidate
    }
  }
  fail('Bun 1.4.2 or newer is not on PATH')
}

function packTarball() {
  const result = run(process.platform === 'win32' ? 'npm.cmd' : 'npm', ['pack', '--ignore-scripts'], {
    cwd: ROOT,
    shell: process.platform === 'win32'
  })
  const name = String(result.stdout || '')
    .trim()
    .split('\n')
    .pop()
  if (!name || !name.endsWith('.tgz')) fail(`npm pack did not name a tarball: ${result.stdout}`)
  return path.join(ROOT, name)
}

function installConsumer(tarball) {
  const consumer = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-bun-packed-consumer-'))
  fs.writeFileSync(
    path.join(consumer, 'package.json'),
    JSON.stringify({ name: 'ubm-bun-packed-consumer', private: true, version: '0.0.0' }, null, 2)
  )
  run(
    process.platform === 'win32' ? 'npm.cmd' : 'npm',
    ['install', '--ignore-scripts', '--omit=optional', '--omit=peer', '--no-audit', '--no-fund', tarball],
    { cwd: consumer, shell: process.platform === 'win32' }
  )
  const installed = path.join(consumer, 'node_modules', 'unified-ble-manager', 'package.json')
  if (!fs.existsSync(installed)) fail(`packed install produced no ${installed}`)
  return consumer
}

function writeProbes(consumer) {
  const cjs = `'use strict'
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const ids = ${JSON.stringify(MANAGER_SCENARIO_IDS)}
const HRM = ${JSON.stringify(HRM)}

function expectedNapi(identitySource) {
  const match = identitySource.match(/napi:\\s*Object\\.freeze\\(\\{\\s*sourceDigest:\\s*'([0-9a-f]+)',\\s*bindingSchema:\\s*'([0-9a-f]+)'/)
  assert.ok(match, 'installed native-build-identity.ts has no napi digest')
  return { sourceDigest: match[1], bindingSchema: match[2] }
}

async function scenarios() {
  const testing = require('unified-ble-manager/testing')
  assert.deepEqual(testing.managerScenarioDefinitions.map(item => item.id), ids)
  const report = await testing.runManagerScenarios(testing.createDeterministicManagerScenarioFactory())
  assert.equal(report.receipts.length, ids.length)
  for (const receipt of report.receipts) {
    assert.equal(receipt.disposition, 'passed', receipt.scenarioId)
  }
}

function publicEntries() {
  const root = require('unified-ble-manager')
  const bluez = require('unified-ble-manager/node/bluez')
  const apple = require('unified-ble-manager/node/corebluetooth')
  const winrt = require('unified-ble-manager/node/winrt')
  assert.equal(typeof root.BleError, 'function')
  assert.equal(typeof bluez.createBluezBleManager, 'function')
  assert.equal(typeof apple.createCoreBluetoothBleManager, 'function')
  assert.equal(typeof winrt.createWinRtBleManager, 'function')
}

async function waker() {
  delete process.env.UBM_NAPI_ADDON
  const packageJson = require.resolve('unified-ble-manager/package.json')
  const packageRoot = path.dirname(packageJson)
  const { loadDesktopCore } = require(path.join(packageRoot, 'native', 'desktop-core', 'index.js'))
  const loaded = loadDesktopCore()
  assert.equal(loaded.mode, 'prebuilt')
  const identity = JSON.parse(loaded.module.nativeBuildIdentity())
  assert.equal(identity.profile, 'release')
  assert.equal(identity.binding, 'napi')
  const expected = expectedNapi(fs.readFileSync(path.join(packageRoot, ${JSON.stringify(INSTALLED_IDENTITY)}), 'utf8'))
  assert.equal(identity.sourceDigest, expected.sourceDigest)
  assert.equal(identity.bindingSchema, expected.bindingSchema)
  const central = await loaded.module.UbmCentral.openSynthetic('bun-packed-desktop', { platform: 'bluez' })
  let wakes = 0
  central.setEventWaker(() => { wakes += 1 })
  const scan = await central.startScan({ owner: 'bun-packed-desktop', serviceUuids: [HRM], timeoutMs: 5000 })
  await central.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Packed', serviceUuids: [HRM] })
  const deadline = Date.now() + 2000
  while (wakes < 1 && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  assert.ok(wakes >= 1, 'event waker did not fire')
  assert.equal(central.eventWakeFailures(), 0)
  await central.stopScan(scan.operationId)
  const closed = await central.close()
  assert.equal(closed.state, 'released')
}

async function main() {
  publicEntries()
  await scenarios()
  await waker()
  process.stdout.write(JSON.stringify({ ok: true, runtime: 'node', scenarios: ids.length }) + '\\n')
}

main().catch(error => {
  console.error(error && error.stack ? error.stack : error)
  process.exit(1)
})
`
  const esm = `import assert from 'node:assert/strict'
const root = await import('unified-ble-manager')
const bluez = await import('unified-ble-manager/node/bluez')
const apple = await import('unified-ble-manager/node/corebluetooth')
const winrt = await import('unified-ble-manager/node/winrt')
const testing = await import('unified-ble-manager/testing')
assert.equal(typeof root.BleError, 'function')
assert.equal(typeof bluez.createBluezBleManager, 'function')
assert.equal(typeof apple.createCoreBluetoothBleManager, 'function')
assert.equal(typeof winrt.createWinRtBleManager, 'function')
assert.deepEqual(testing.managerScenarioDefinitions.map(item => item.id), ${JSON.stringify(MANAGER_SCENARIO_IDS)})
const report = await testing.runManagerScenarios(testing.createDeterministicManagerScenarioFactory())
if (report.receipts.length !== ${MANAGER_SCENARIO_IDS.length} || report.receipts.some(receipt => receipt.disposition !== 'passed')) {
  throw new Error('ESM manager scenarios did not all pass')
}
process.stdout.write(JSON.stringify({ ok: true, runtime: 'esm' }) + '\\n')
`
  fs.writeFileSync(path.join(consumer, 'probe.cjs'), cjs)
  fs.writeFileSync(path.join(consumer, 'probe.mjs'), esm)
}

function main() {
  const bun = bunBinary()
  const tarball = packTarball()
  try {
    const consumer = installConsumer(tarball)
    writeProbes(consumer)
    const env = { ...process.env }
    delete env.UBM_NAPI_ADDON
    run(process.execPath, ['probe.cjs'], { cwd: consumer, env })
    run(process.execPath, ['probe.mjs'], { cwd: consumer, env })
    run(bun, ['probe.cjs'], { cwd: consumer, env })
    run(bun, ['probe.mjs'], { cwd: consumer, env })
    process.stdout.write(
      `${JSON.stringify({
        ok: true,
        bun,
        scenarios: MANAGER_SCENARIO_IDS,
        consumer
      })}\n`
    )
  } finally {
    fs.rmSync(tarball, { force: true })
  }
}

module.exports = { MANAGER_SCENARIO_IDS }

if (require.main === module) {
  main()
}
