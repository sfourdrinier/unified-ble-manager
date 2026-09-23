// scripts/ci/f01-factory-routing-proof.cjs
//
// F01 Leg G (SEAM PROOF over the PACKED package; never a device or radio
// claim): the REAL production factories of the installed package route
// through the Rust-core boundary, with no TypeScript or legacy fallback.
//
// REAL in this proof: the packed `lib/` factory code, the production
// `UnifiedBleRustCore` TurboModule resolution (no `rustCore` injection), the
// production binding and `ubm-mobile-wire/1` codec, the packed sealed build
// identity, and a throwing legacy `UnifiedBleProtocolControl` (any legacy-route
// work throws and fails the proof).
//
// SYNTHETIC in this proof: `react-native` Platform/TurboModuleRegistry are
// stubbed; the `UnifiedBleRustCore` module is the deterministic wire double
// (test-support/react-native/deterministic-rust-core-native.js), whose
// argument key sets are pinned to the Rust owner's own golden vectors by
// __tests__/backends/reactnative/rust-core-double-schema.test.js; the Tauri
// leg replays a stub transport (revision admission only). The real Rust owner
// behind the same wire is exercised by the JVM exchange
// (bindings/jni/run_mobile_roundtrip.sh) and the Apple harness; physical
// evidence is the R01FACTORY device leg.
//
// PR210-55: the previous version drove an injected binding of the removed
// shape (`contractRevision()`, `scan.take`, `notifications.take`,
// `events.take`) over the NAPI synthetic central. That shape no longer
// exists: React Native reaches the Rust owner only through the TurboModule.
//
// Usage: node scripts/ci/f01-factory-routing-proof.cjs <installed-dir>
// Exit non-zero with `F01-LEG-G:` diagnostics on any failure.

'use strict'

const path = require('path')

const HRM_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HRM_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const repoRoot = path.resolve(__dirname, '..', '..')

function fail(step, detail) {
  console.error(`F01-LEG-G: FAIL ${step}: ${detail}`)
  process.exitCode = 1
  throw new Error(`F01-LEG-G ${step}: ${detail}`)
}

function check(step, condition, detail) {
  if (!condition) fail(step, detail)
  console.log(`F01-LEG-G: ok ${step}`)
}

function throwingModule(name) {
  return new Proxy(
    {},
    {
      get: (_target, property) => {
        throw new Error(`${name} executed BLE work on the Rust route (touched ${String(property)})`)
      }
    }
  )
}

async function takeValue(stream, what, timeoutMs = 15000) {
  // A harness awaiting a value holds its own runloop: this ref'd watchdog
  // turns a parked stream into a loud failure instead of a silent exit-0.
  let timer = null
  const watchdog = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`stream ${what} produced no value within ${timeoutMs}ms`)), timeoutMs)
  })
  const take = (async () => {
    for await (const item of stream) {
      if (item && (item.kind === 'value' || item.value !== undefined))
        return item.value !== undefined ? item.value : item
    }
    throw new Error(`stream ${what} terminated without a value`)
  })()
  try {
    return await Promise.race([take, watchdog])
  } catch (error) {
    fail('rn-public-manager', `stream ${what}: ${error && error.message ? error.message : error}`)
    throw new Error('unreachable')
  } finally {
    if (timer !== null) clearTimeout(timer)
  }
}

async function proveReactNative(installed) {
  const { DeterministicRustCoreNative, DEFAULT_PEER } = require(
    path.join(repoRoot, 'test-support', 'react-native', 'deterministic-rust-core-native.js')
  )
  const { EXPECTED_NATIVE_BUILD_IDENTITY } = require(
    path.join(installed, 'lib', 'commonjs', 'generated', 'native-build-identity.js')
  )
  const control = throwingModule('UnifiedBleProtocolControl')
  let native = null
  const Module = require('module')
  const originalLoad = Module._load
  Module._load = function hooked(request, parent, isMain) {
    if (request === 'react-native') {
      return {
        Platform: { OS: 'android', Version: 35, select: options => options.android },
        TurboModuleRegistry: {
          get: name => (name === 'UnifiedBleRustCore' ? native : name === 'UnifiedBleProtocolControl' ? control : null),
          getEnforcing: name => {
            if (name === 'UnifiedBleProtocolControl') return control
            if (name === 'UnifiedBleRustCore' && native !== null) return native
            throw new Error(`TurboModule ${name} is not installed`)
          }
        },
        NativeModules: {}
      }
    }
    return originalLoad.call(this, request, parent, isMain)
  }
  try {
    const lib = require(path.join(installed, 'lib', 'commonjs', 'react-native.js'))
    check(
      'rn-factory-present',
      typeof lib.createReactNativeBleManager === 'function',
      'packed lib must export createReactNativeBleManager'
    )
    native = new DeterministicRustCoreNative({ platform: 'android', expectedIdentity: EXPECTED_NATIVE_BUILD_IDENTITY })
    const ops = op => native.opsInvoked(op).length
    // The ordinary factory: no `rustCore` option, the production module resolution.
    const manager = await lib.createReactNativeBleManager()
    check('rn-factory-creates', manager !== null && manager !== undefined, 'the ordinary factory must create a manager')
    check(
      'rn-session-admitted',
      native.calls.some(call => call[0] === 'openSession' && call[2] === 'ubm-mobile-wire/1'),
      'creation must admit one ubm-mobile-wire/1 session on UnifiedBleRustCore'
    )
    check(
      'rn-entropy-native',
      native.calls.some(call => call[0] === 'randomBytes'),
      'host entropy comes from the module'
    )

    const state = await manager.adapter.state()
    check(
      'rn-adapter-state',
      state !== null && state !== undefined && ops('adapter.state') >= 1,
      'adapter state routes'
    )

    const scan = await manager.scan({ query: { anyOf: [{ services: { any: [HRM_SERVICE] } }] }, duplicates: 'all' })
    check('rn-scan-routes', ops('scan.start') === 1, 'public scan must dispatch scan.start')
    native.emitAdvertisement()
    const observation = await takeValue(scan.observations, 'scan observations')
    check(
      'rn-scan-observes',
      observation && observation.peer && observation.peer.id,
      'scan must yield the emitted peer'
    )
    await scan.stop()
    check('rn-scan-stops', ops('scan.stop') === 1, 'scan stop must dispatch scan.stop')

    const connection = await manager.connect(observation.peer.id, {})
    check('rn-connect-routes', ops('connection.connect') === 1, 'connect must dispatch connection.connect')
    const database = await connection.discover()
    check('rn-discover-routes', ops('gatt.discover') === 1, 'discover must dispatch gatt.discover')
    const characteristic = database.characteristic(HRM_SERVICE, HRM_MEASUREMENT)
    const value = await characteristic.read()
    check('rn-read-routes', ops('gatt.read') === 1 && value && value.length === 2, 'read must return the owner bytes')
    await characteristic.write(new Uint8Array([0x01]), { response: 'required' })
    check('rn-write-routes', ops('gatt.write') === 1, 'write must dispatch gatt.write')
    const subscription = await characteristic.subscribe()
    check('rn-subscribe-routes', ops('gatt.subscribe') === 1, 'subscribe must dispatch gatt.subscribe')
    native.emitNotification(new Uint8Array([0x06, 0x40]), DEFAULT_PEER)
    const notification = await takeValue(subscription.values, 'notifications')
    check(
      'rn-notifies',
      notification && notification.value && [...notification.value].join(',') === '6,64',
      `expected notification 6,64, got ${notification && notification.value && [...notification.value].join(',')}`
    )
    await subscription.remove()
    check('rn-unsubscribe-routes', ops('gatt.unsubscribe') === 1, 'unsubscribe must dispatch gatt.unsubscribe')
    await connection.disconnect()
    check('rn-disconnect-routes', ops('connection.disconnect') === 1, 'disconnect must dispatch connection.disconnect')
    await manager.destroy()
    check('rn-dispose-routes', ops('session.dispose') === 1, 'destroy must dispose the session')
    check(
      'rn-lease-closed',
      native.calls.some(call => call[0] === 'closeSession'),
      'destroy must close the session lease'
    )

    // A foreign native build fails closed before any session opens; there is
    // no other route to fall back to.
    native = new DeterministicRustCoreNative({ platform: 'android', expectedIdentity: EXPECTED_NATIVE_BUILD_IDENTITY })
    native.identity = { ...native.identity, bindingSchema: '0'.repeat(64) }
    const foreign = await lib.createReactNativeBleManager().then(
      () => null,
      error => error
    )
    check(
      'rn-identity-gate',
      foreign !== null && String(foreign.code || foreign.message).includes('protocol.incompatible'),
      `a foreign native build must fail closed with protocol.incompatible, got ${foreign && (foreign.code || foreign.message)}`
    )
    check(
      'rn-identity-gate-no-session',
      !native.calls.some(call => call[0] === 'openSession'),
      'a foreign native build must be refused before any session opens'
    )

    // No module at all: capability.unsupported, never a TypeScript radio.
    native = null
    const missing = await lib.createReactNativeBleManager().then(
      () => null,
      error => error
    )
    check(
      'rn-no-fallback',
      missing !== null && String(missing.code || missing.message).includes('capability.unsupported'),
      `a missing UnifiedBleRustCore must fail with capability.unsupported, got ${missing && (missing.code || missing.message)}`
    )
  } finally {
    Module._load = originalLoad
  }
}

// -- Tauri: real factory against a stub transport ----------------------------

class ProofChannel {
  constructor() {
    this.onmessage = null
  }
}

function negotiated(axis, value) {
  const selected = { axis, value }
  const range = { axis, minimum: { ...selected }, maximum: { ...selected } }
  return { axis, selected, localRange: { ...range }, remoteRange: { ...range } }
}

function proofCapabilityDescriptor(id, scenario, state) {
  const limitation = {
    code: state === 'limited' ? 'deterministic-only' : 'not-implemented',
    explanation:
      state === 'limited'
        ? 'The proof stub exposes deterministic host evidence only.'
        : 'The proof stub does not implement this capability.',
    affectedGuarantee: state === 'limited' ? 'Physical-radio qualification is not claimed.' : 'support'
  }
  const schemaRange = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  return {
    id,
    state,
    selectedSchemaRange: schemaRange,
    implementationOrigin: 'backend-native',
    tck: { suiteId: 'capability.catalog-v2', requiredScenarioIds: [scenario], contractRange: schemaRange },
    evidence: {
      receiptId: `leg-g-${id}`,
      evidenceLevel: state === 'limited' ? 'deterministic' : 'blocked',
      implementationVersion: 'leg-g-stub-v2',
      sourceDigest: `leg-g-${id}`,
      scenarioIds: [scenario],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
  }
}

// Complete capability catalog, mirroring the __tests__/TauriManager.test.js
// stub fixture: the factory validates catalog-completeness against
// BUILT_IN_FEATURE_CATALOG, so a stub transport must replay every built-in
// id (as that fixture does). Reads the id list from the PACKED contract so
// the stub stays in sync; the Leg G assertions (routing, revision gate,
// no-silent-fallback) do not depend on capability values.
function proofCapabilitySnapshot(installed, backendGeneration) {
  const packed = require(path.join(installed, 'lib', 'commonjs', 'backend-contract', 'capabilities.js'))
  const ids = Object.values(packed.BUILT_IN_FEATURE_IDS)
  if (!Array.isArray(ids) || ids.length === 0) fail('tauri-stub', 'packed contract must export BUILT_IN_FEATURE_IDS')
  const limited = new Map([
    ['discovery:continuous-scan', 'scan.owner-join-authority-and-signature'],
    ['connection:direct', 'connection.lease-joins-borrowing-transfer-and-revocation'],
    ['connection:rssi', 'connection.rssi-and-att-mtu-capability-contract'],
    ['gatt:descriptors', 'gatt.descriptor-discovery-read-write'],
    ['gatt:indications', 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation']
  ])
  return {
    schemaVersion: 2,
    backendGeneration,
    descriptors: ids.map(id => {
      const scenario = limited.get(id)
      return proofCapabilityDescriptor(
        id,
        scenario ?? 'capability.truth-limits-evidence-and-binding',
        scenario === undefined ? 'unsupported' : 'limited'
      )
    })
  }
}

function proofBootstrap(contractRevision, capabilities) {
  const backendGeneration = 'leg-g-backend-generation'
  return {
    attachment: {
      attachmentId: 'leg-g-attachment',
      backendInstanceId: 'leg-g-backend',
      backendGeneration,
      adapter: {
        adapterId: 'leg-g-adapter',
        displayName: 'LegG',
        state: {
          availability: 'available',
          authorization: 'granted',
          power: 'on',
          heard: null,
          backendGeneration,
          updatedAt: 1,
          safeReason: null
        },
        adapterGeneration: 'leg-g-adapter-generation',
        limitations: []
      }
    },
    attachmentId: 'leg-g-attachment',
    versions: {
      backendContract: negotiated('backend-contract', 1),
      capabilitySchema: negotiated('capability-schema', 1),
      eventSchema: negotiated('event-schema', 1),
      traceFormat: negotiated('trace-format', 1),
      ipcProtocol: negotiated('ipc-protocol', 3)
    },
    capabilities,
    core: { contractRevision, implementationVersion: '5.0.0-rc.2' },
    renderer: { clientId: 'leg-g-client', windowScope: 'main', sessionScope: 'leg-g-scope' },
    rendererLease: { leaseId: 'leg-g-lease', generation: 'leg-g-lease-generation' }
  }
}

async function proveTauri(installed, packedRevision) {
  const lib = require(path.join(installed, 'lib', 'commonjs', 'tauri.js'))
  check(
    'tauri-factory-present',
    typeof lib.createTauriBleManagerWithEnvironment === 'function',
    'packed lib must export createTauriBleManagerWithEnvironment'
  )
  const invocations = []
  const capabilities = proofCapabilitySnapshot(installed, 'leg-g-backend-generation')
  const environment = {
    invoke: async (command, args) => {
      invocations.push({ command, args })
      const request = args.request
      if (request.kind === 'bootstrap')
        return { kind: 'bootstrap', bootstrap: proofBootstrap(packedRevision, capabilities) }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`F01-LEG-G: unexpected Tauri route ${request.envelope && request.envelope.command}`)
    },
    Channel: ProofChannel
  }
  const manager = await lib.createTauriBleManagerWithEnvironment(environment, {})
  check('tauri-factory-creates', manager !== null && manager !== undefined, 'real Tauri factory must create a manager')
  check(
    'tauri-bootstrap-invoked',
    invocations.some(entry => entry.args && entry.args.request && entry.args.request.kind === 'bootstrap'),
    'factory must invoke the transport bootstrap (no silent fallback)'
  )
  await manager.destroy()

  // The repo contract for a foreign host revision is the rejection the
  // committed TauriManager tests pin: the factory throws matching
  // /contract revision/i AND releases the rejected host (bootstrap then
  // release, never a silent fallback manager). That is the fail-closed
  // property under proof here (the RN lane carries the same gate as
  // protocol.incompatible; each lane asserts its own committed identity).
  const foreignInvocations = []
  const foreign = await lib
    .createTauriBleManagerWithEnvironment(
      {
        invoke: async (command, args) => {
          const request = args.request
          foreignInvocations.push(request.kind)
          if (request.kind === 'bootstrap') {
            return { kind: 'bootstrap', bootstrap: proofBootstrap('C-UBM.9.9.9-DRAFT', capabilities) }
          }
          if (request.kind === 'event.ack') return { kind: 'event.ack' }
          if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
          throw new Error(`F01-LEG-G: unexpected Tauri route ${request.envelope && request.envelope.command}`)
        },
        Channel: ProofChannel
      },
      {}
    )
    .then(
      () => null,
      error => error
    )
  check(
    'tauri-revision-gate',
    foreign !== null && /contract revision/i.test(String((foreign && foreign.message) || foreign)),
    'foreign plugin revision must fail closed matching /contract revision/i'
  )
  check(
    'tauri-revision-releases',
    JSON.stringify(foreignInvocations) === JSON.stringify(['bootstrap', 'release']),
    `rejected foreign host must be released, saw [${foreignInvocations.join(',')}]`
  )

  const missing = await Promise.resolve()
    .then(() => lib.createTauriBleManager({}))
    .then(
      () => null,
      error => error
    )
  check('tauri-no-silent-fallback', missing !== null, 'createTauriBleManager without Tauri globals must throw loudly')
}

// -- entry -------------------------------------------------------------------

async function main() {
  const installed = process.argv[2]
  if (!installed) {
    console.error('F01-LEG-G: usage: f01-factory-routing-proof.cjs <installed-dir>')
    process.exit(2)
  }
  let packedRevision = null
  try {
    const seam = require(
      path.join(installed, 'lib', 'commonjs', 'backends', 'reactnative', 'react-native-rust-core.js')
    )
    packedRevision = seam.RUST_CORE_CONTRACT_REVISION
  } catch (error) {
    fail('packed-revision', `the packed lib must expose RUST_CORE_CONTRACT_REVISION: ${error && error.message}`)
  }
  check('packed-revision', typeof packedRevision === 'string' && packedRevision.length > 0, 'packed contract revision')
  console.log(`F01-LEG-G: packed contract revision ${packedRevision}`)

  await proveReactNative(installed)
  await proveTauri(installed, packedRevision)
  console.log('F01-LEG-G: OK (real factories route through the Rust core; fallbacks fail loudly)')
}

main().catch(error => {
  console.error(`F01-LEG-G: FAIL ${error && error.message ? error.message : error}`)
  process.exitCode = 1
})
