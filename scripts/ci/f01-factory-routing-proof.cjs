// scripts/ci/f01-factory-routing-proof.cjs
//
// F01 Leg G: the REAL production factories route through the shared Rust
// core, and any TypeScript fallback fails the proof loudly.
//
// This is the exact cited defect the previous acceptance run missed: Leg G
// checked `'UbmCentral' in m` without ever invoking
// `createReactNativeBleManager` / `createTauriBleManager`. Here both real
// factories are invoked:
//
//   RN:    the real `createReactNativeBleManager` (react-native Platform
//          stubbed, the generated protocol control replaced by a throwing
//          proxy) is constructed with a binding implemented over the PACKED
//          NAPI addon (`UbmCentral.openSynthetic`, which genuinely executes
//          DesktopCentral/ubm-core). Manager creation, scan, connect,
//          discover, read, write, subscribe, notification take, unsubscribe,
//          disconnect, and destroy all run through the public manager; every
//          test then asserts the NAPI core saw the op AND the throwing
//          control was never touched. Any TypeScript-fallback BLE work
//          throws through the proxy and fails the proof.
//   Tauri: the real `createTauriBleManagerWithEnvironment` runs against a
//          stub transport replaying the PACKED contract revision: admission
//          succeeds only against the real plugin identity, and a foreign
//          revision fails closed matching /contract revision/i AND releases
//          the rejected host (the identity the committed TauriManager tests
//          pin; no silent fallback manager). `createTauriBleManager`
//          without an environment must throw loudly (missing Tauri globals),
//          never return a fake.
//
// Usage: node scripts/ci/f01-factory-routing-proof.cjs <installed-dir> <addon-path>
// Exit non-zero with `F01-LEG-G:` diagnostics on any failure.

'use strict'

const path = require('path')

const HRM_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HRM_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const HRM_BODY_LOCATION = '00002a38-0000-1000-8000-00805f9b34fb'
const PEER_ID = 'aa:bb:cc:dd:ee:ff'

function fail(step, detail) {
  console.error(`F01-LEG-G: FAIL ${step}: ${detail}`)
  process.exitCode = 1
  throw new Error(`F01-LEG-G ${step}: ${detail}`)
}

function check(step, condition, detail) {
  if (!condition) fail(step, detail)
  console.log(`F01-LEG-G: ok ${step}`)
}

function requirePacked(installed, subpath) {
  return require(path.join(installed, subpath))
}

function throwingControl() {
  return new Proxy(
    {},
    {
      get: (_target, property) => {
        throw new Error(`TypeScript control surface executed BLE work (touched ${String(property)})`)
      },
      apply: () => {
        throw new Error('TypeScript control surface executed BLE work (called)')
      }
    }
  )
}

// -- NAPI-backed Rust core binding (F01 op contract over UbmCentral) --------

function selectorOf(selector) {
  // Mirror the real dispatch contract (bindings/napi/src/dispatch.rs):
  // characteristic ops carry exactly the four characteristic fields; the
  // descriptor keys exist only on descriptor ops. The addon resolves a
  // selector carrying an explicit `descriptorUuid: undefined` as a
  // descriptor path and fails closed with gatt.not-found, so never send
  // the keys unless a descriptor op provides them.
  const shaped = {
    serviceUuid: selector.serviceUuid,
    serviceOccurrence: selector.serviceOccurrence ?? 0,
    characteristicUuid: selector.characteristicUuid,
    characteristicOccurrence: selector.characteristicOccurrence ?? 0
  }
  if (selector.descriptorUuid !== undefined && selector.descriptorUuid !== null) {
    shaped.descriptorUuid = selector.descriptorUuid
    shaped.descriptorOccurrence = selector.descriptorOccurrence ?? 0
  }
  return shaped
}

function createNapiBinding(sharedCentral, revision, log) {
  return {
    openSession: async owner => {
      if (typeof owner !== 'string' || owner.length === 0) throw new Error('owner must not be empty')
      // One shared synthetic central per proof: the manager's session
      // executes the SAME DesktopCentral the proof staged, so staged
      // advertisements/services/notifications are genuinely observed.
      // dispose runs the real destroy transition (central close, idempotent);
      // close repeats it idempotently.
      const central = sharedCentral
      const bytesOf = value => (Buffer.isBuffer(value) ? new Uint8Array(value) : value)
      return {
        contractRevision: () => revision,
        invoke: async (op, args) => {
          log.push(op)
          switch (op) {
            case 'adapter.state':
              return {
                availability: 'available',
                authorization: 'unknown',
                power: 'on',
                backendGeneration: 'gen-1',
                updatedAt: Date.now(),
                safeReason: null
              }
            case 'counters.describe':
              return {
                activeScanControllers: 0,
                scanConsumers: 0,
                chooserSessions: 0,
                connectionLeases: 0,
                physicalLinks: 0,
                databaseSnapshots: 0,
                physicalCccdEnablements: 0,
                subscriptionConsumers: 0,
                queuedOperations: 0,
                dispatchedOperations: 0,
                retainedByteBuffers: 0,
                restorationRecords: 0,
                orphanedIpcOwners: 0
              }
            case 'scan.start':
              await central.startScan({ owner: 'leg-g', serviceUuids: args.serviceUuids, timeoutMs: args.timeoutMs ?? 5000 })
              return { operationId: 'leg-g-scan' }
            case 'scan.take': {
              const next = await central.takeAdvertisement()
              if (next === null || next === undefined) return null
              return {
                peerId: next.peerId,
                rssi: next.rssi ?? null,
                localName: next.localName ?? null,
                serviceUuids: next.serviceUuids ?? [],
                manufacturerData: (next.manufacturerData ?? []).map(entry => ({
                  companyId: entry.companyId,
                  payload: bytesOf(entry.payload)
                })),
                serviceData: (next.serviceData ?? []).map(entry => ({
                  uuid: entry.uuid,
                  payload: bytesOf(entry.payload)
                })),
                txPower: next.txPower ?? null,
                connectable: null
              }
            }
            case 'scan.stop':
              await central.stopScan()
              return { state: 'released' }
            case 'connection.connect': {
              const info = await central.connect({ peerId: args.peerId, lease: args.lease, timeoutMs: args.timeoutMs ?? 5000 })
              return { peerKey: info.peerKey, connectionGeneration: info.connectionGeneration ?? 'gen-0' }
            }
            case 'connection.disconnect':
              await central.disconnect({ peerId: args.peerId, lease: args.lease })
              return {}
            case 'gatt.discover': {
              await central.discover({ peerId: args.peerId, lease: args.lease })
              const paths = await central.discoveredPaths(args.peerId)
              const services = new Map()
              for (const entry of paths) {
                if (entry.descriptorUuid) continue
                let service = services.get(`${entry.serviceUuid}#${entry.serviceOccurrence ?? 0}`)
                if (!service) {
                  service = { uuid: entry.serviceUuid, occurrence: entry.serviceOccurrence ?? 0, characteristics: [] }
                  services.set(`${entry.serviceUuid}#${entry.serviceOccurrence ?? 0}`, service)
                }
                if (entry.characteristicUuid) {
                  let characteristic = service.characteristics.find(
                    item =>
                      item.uuid === entry.characteristicUuid &&
                      item.occurrence === (entry.characteristicOccurrence ?? 0)
                  )
                  if (!characteristic) {
                    characteristic = {
                      uuid: entry.characteristicUuid,
                      occurrence: entry.characteristicOccurrence ?? 0,
                      properties: entry.properties ?? 0,
                      descriptors: []
                    }
                    service.characteristics.push(characteristic)
                  }
                  if (entry.descriptorUuid) {
                    characteristic.descriptors.push({
                      uuid: entry.descriptorUuid,
                      occurrence: entry.descriptorOccurrence ?? 0
                    })
                  }
                }
              }
              return { services: [...services.values()] }
            }
            case 'gatt.read': {
              const value = await central.read({
                peerId: args.peerId,
                selector: selectorOf(args.selector),
                timeoutMs: args.timeoutMs ?? 5000
              })
              return { value: bytesOf(value) }
            }
            case 'gatt.write':
              await central.write({
                peerId: args.peerId,
                selector: selectorOf(args.selector),
                value: Buffer.from(args.value),
                mode: args.mode,
                timeoutMs: args.timeoutMs ?? 5000
              })
              return {}
            case 'gatt.subscribe':
              await central.subscribe({
                peerId: args.peerId,
                selector: selectorOf(args.selector),
                consumer: args.consumer,
                timeoutMs: args.timeoutMs ?? 5000
              })
              return {}
            case 'notifications.take': {
              const value = await central.takeNotification({
                peerId: args.peerId,
                selector: selectorOf(args.selector),
                consumer: args.consumer
              })
              return value === null || value === undefined ? null : { value: bytesOf(value) }
            }
            case 'gatt.unsubscribe': {
              const disabled = await central.unsubscribe({
                peerId: args.peerId,
                selector: selectorOf(args.selector),
                consumer: args.consumer
              })
              return { disabled }
            }
            case 'peers.resolve':
              return null
            case 'peers.known':
            case 'peers.connected':
              return []
            case 'events.take':
              return null
            case 'op.cancel':
              return { state: 'not-cancellable' }
            case 'session.dispose':
              await central.close()
              return { state: 'released' }
            default:
              throw new Error(`F01-LEG-G: binding has no mapping for core op ${op}`)
          }
        },
        close: async () => {
          if (central !== null && central !== undefined) {
            await central.close().catch(() => undefined)
          }
        }
      }
    }
  }
}

async function takeValue(stream, what, timeoutMs = 15000) {
  // The provider's pumps pace with unref'd delays by design (they must never
  // hold a process or test runner open): a harness awaiting a value must hold
  // its own runloop. This ref'd watchdog does that, and turns a parked
  // stream into a loud failure instead of a silent exit-0 with no verdict
  // (Node exits 0 on a drained loop even with unsettled promises).
  let timer = null
  const watchdog = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`stream ${what} produced no value within ${timeoutMs}ms`)), timeoutMs)
  })
  const take = (async () => {
    for await (const item of stream) {
      if (item && (item.kind === 'value' || item.value !== undefined)) return item.value !== undefined ? item.value : item
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

async function proveReactNative(installed, UbmCentral, packedRevision) {
  // Stub react-native BEFORE requiring the packed app factory: the factory
  // infers the platform from Platform.OS, nothing else.
  // The throwing control must exist before the hook: the generated spec
  // resolves its default export at load time via getEnforcing.
  const control = throwingControl()
  const Module = require('module')
  const originalLoad = Module._load
  Module._load = function hooked(request, parent, isMain) {
    if (request === 'react-native') {
      return {
        Platform: { OS: 'android', select: options => options.android },
        TurboModuleRegistry: {
          get: () => control,
          getEnforcing: () => control
        }
      }
    }
    return originalLoad.call(this, request, parent, isMain)
  }
  // The hook stays installed for the whole RN section: the app factory
  // lazily requires react-native (TurboModule control) at creation time,
  // and the real package is not loadable headless.
  const lib = require(path.join(installed, 'lib', 'commonjs', 'react-native.js'))
  check('rn-factory-present', typeof lib.createReactNativeBleManager === 'function', 'packed lib must export createReactNativeBleManager')

  const log = []
  // Stage the shared synthetic radio BEFORE the manager touches it.
  const shared = await UbmCentral.openSynthetic('leg-g-shared')
  await shared.stageAdvertisement({ peerId: PEER_ID, rssi: -60, localName: 'LegG', serviceUuids: [HRM_SERVICE] })
  await shared.stageServices(PEER_ID, [
    {
      uuid: HRM_SERVICE,
      occurrence: 0,
      characteristics: [
        {
          uuid: HRM_MEASUREMENT,
          occurrence: 0,
          properties: { read: true, write: false, writeWithoutResponse: false, notify: true, indicate: false },
          descriptors: []
        },
        // Writes need a writable property (enforced in Rust: the notify-only
        // measurement rejects): the body-location characteristic carries the
        // proof write, exactly like dispatch_roundtrip.cjs stages it.
        {
          uuid: HRM_BODY_LOCATION,
          occurrence: 0,
          properties: { read: true, write: true, writeWithoutResponse: true, notify: false, indicate: false },
          descriptors: []
        }
      ]
    }
  ])
  // Writes need a measured MTU, enforced in Rust (dispatch_roundtrip.cjs):
  // without it the core fails closed with capability.unavailable.
  await shared.stageMtu(PEER_ID, 128)
  // NOTE: the notification is staged AFTER subscribe below. The synthetic
  // radio only delivers to an existing subscriber (dispatch_roundtrip.cjs
  // subscribes first, then stages): staging upfront parks the take forever
  // and Node exits 0 on the drained loop with no verdict.
  const binding = createNapiBinding(shared, packedRevision, log)

  // The REAL app factory (not the WithEnvironment seam): fail loudly if the
  // TypeScript control surface executes any BLE work. Explicit test entropy
  // keeps even host identity off the native module.
  const manager = await lib.createReactNativeBleManager({
    randomBytes: length => new Uint8Array(length).fill(7),
    rustCore: binding
  })
  check('rn-factory-creates', manager !== null && manager !== undefined, 'real factory must create a manager')
  check('rn-core-admission', log.includes('adapter.state'), 'creation must admit through the core (adapter.state)')

  const state = await manager.adapter.state()
  check('rn-adapter-state', state !== null && state !== undefined, 'adapter state must resolve through the core')

  const session = await manager.scan({
    query: { anyOf: [{ services: { any: [HRM_SERVICE] } }] },
    duplicates: 'all'
  })
  check('rn-scan-routes', log.includes('scan.start'), 'public scan must dispatch scan.start to the core')
  const observation = await takeValue(session.observations, 'scan observations')
  check('rn-scan-observes', observation && observation.peer && observation.peer.id, 'scan must yield the staged peer')
  await session.stop()
  check('rn-scan-stops', log.includes('scan.stop'), 'scan stop must dispatch scan.stop to the core')

  const connection = await manager.connect(observation.peer.id, {})
  check('rn-connect-routes', log.includes('connection.connect'), 'connect must dispatch connection.connect')
  const database = await connection.discover()
  check('rn-discover-routes', log.includes('gatt.discover'), 'discover must dispatch gatt.discover')
  const services = database.servicesByUuid(HRM_SERVICE)
  check('rn-discovers-tree', services.length === 1, `expected 1 staged service, got ${services.length}`)
  const characteristic = database.characteristic(HRM_SERVICE, HRM_MEASUREMENT)
  const value = await characteristic.read()
  check('rn-read-routes', log.includes('gatt.read') && value && value.length > 0, 'read must dispatch gatt.read and return bytes')
  const writable = database.characteristic(HRM_SERVICE, HRM_BODY_LOCATION)
  await writable.write(new Uint8Array([0x01]), { response: 'not-required' })
  check('rn-write-routes', log.includes('gatt.write'), 'write must dispatch gatt.write')
  const subscription = await characteristic.subscribe()
  check('rn-subscribe-routes', log.includes('gatt.subscribe'), 'subscribe must dispatch gatt.subscribe')
  await shared.stageNotification({
    peerId: PEER_ID,
    serviceUuid: HRM_SERVICE,
    characteristicUuid: HRM_MEASUREMENT,
    value: Buffer.from([0x06, 0x40])
  })
  const notification = await takeValue(subscription.values, 'notifications')
  check(
    'rn-notifies',
    notification && notification.value && [...notification.value].join(',') === '6,64',
    `expected staged notification 6,64, got ${notification && notification.value && [...notification.value].join(',')}`
  )
  await subscription.remove()
  await connection.disconnect()
  check('rn-disconnect-routes', log.includes('connection.disconnect'), 'disconnect must dispatch connection.disconnect')
  await manager.destroy()
  check('rn-dispose-routes', log.includes('session.dispose'), 'destroy must dispatch session.dispose to the core')

  // Revision gate through the REAL factory: a foreign core fails closed,
  // never substitutes the TypeScript manager.
  const foreign = await lib
    .createReactNativeBleManager({
      randomBytes: length => new Uint8Array(length).fill(7),
      rustCore: createNapiBinding(null, 'C-UBM.9.9.9-DRAFT', [])
    })
    .then(
      () => null,
      error => error
    )
  check(
    'rn-revision-gate',
    foreign !== null && String((foreign && foreign.message) || foreign).includes('protocol.incompatible'),
    'foreign core revision must fail closed with protocol.incompatible'
  )
  Module._load = originalLoad
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
      ipcProtocol: negotiated('ipc-protocol', 2)
    },
    capabilities,
    core: { contractRevision, implementationVersion: '5.0.0-rc.0' },
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
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: proofBootstrap(packedRevision, capabilities) }
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
  const addonPath = process.argv[3]
  if (!installed || !addonPath) {
    console.error('F01-LEG-G: usage: f01-factory-routing-proof.cjs <installed-dir> <addon-path>')
    process.exit(2)
  }
  const { UbmCentral } = require(addonPath)
  check('napi-addon-loads', typeof UbmCentral.openSynthetic === 'function', 'packed addon must expose UbmCentral.openSynthetic')

  let packedRevision = 'C-UBM.0.1.2-DRAFT'
  try {
    const seam = require(
      path.join(installed, 'lib', 'commonjs', 'backends', 'reactnative', 'react-native-rust-core.js')
    )
    if (seam && typeof seam.RUST_CORE_CONTRACT_REVISION === 'string') {
      packedRevision = seam.RUST_CORE_CONTRACT_REVISION
    }
  } catch {
    // Fall back to the frozen default (pinned by UbmAarJniParityTest too).
  }
  console.log(`F01-LEG-G: packed contract revision ${packedRevision}`)

  await proveReactNative(installed, UbmCentral, packedRevision)
  await proveTauri(installed, packedRevision)
  console.log('F01-LEG-G: OK (real factories route through the shared core; fallbacks fail loudly)')
}

main().catch(error => {
  console.error(`F01-LEG-G: FAIL ${error && error.message ? error.message : error}`)
  process.exitCode = 1
})
