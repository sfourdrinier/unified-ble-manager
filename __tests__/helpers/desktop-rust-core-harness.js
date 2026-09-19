// __tests__/helpers/desktop-rust-core-harness.js
//
// Harness for the desktop Rust core tests: the REAL N-API addon (built by
// `pretest:package` -> scripts/ci/build-napi-addon.js) on its deterministic
// synthetic radio. Every operation executes DesktopCentral in Rust; the spy
// only records which UbmCentral methods TypeScript called, so option-audit
// tests can assert "zero dispatch calls" and Rust-ingress counts.

'use strict'

const fs = require('node:fs')
const path = require('node:path')

const { bindDesktopCore } = require('../../src/desktop-core-addon')
const {
  DESKTOP_RUST_CORE_PROFILES,
  createTestDesktopRustCoreBackendProvider
} = require('../../src/backends/desktop/desktop-rust-core-provider')
const { capacity } = require('../../src/backend-contract/primitives')

const HRM_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HRM_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const HRM_CONTROL = '00002a39-0000-1000-8000-00805f9b34fb'
const USER_DESCRIPTION = '00002901-0000-1000-8000-00805f9b34fb'

const addonPath =
  process.env.UBM_NAPI_ADDON ??
  path.join(__dirname, '..', '..', 'bindings', 'napi', `ubm_echo.${process.platform}-${process.arch}.node`)

function loadAddon() {
  if (!fs.existsSync(addonPath)) {
    throw new Error(`desktop core addon missing at ${addonPath}: run node scripts/ci/build-napi-addon.js`)
  }
  return require(addonPath)
}

const HOST_PLATFORM = Object.freeze({ bluez: 'linux', corebluetooth: 'darwin', winrt: 'win32' })

/** Wrap a native central so every method call is recorded (this stays the native object). */
function spyCentral(central, calls) {
  return new Proxy(central, {
    get(target, property) {
      const value = Reflect.get(target, property)
      if (typeof value !== 'function') return value
      return (...args) => {
        calls.push([String(property), args])
        return Reflect.apply(value, target, args)
      }
    }
  })
}

/**
 * The identity-verified binding over the real addon (source mode: a debug
 * checkout build, digests still checked). `openProduction` is served by the
 * synthetic radio so public factories can be exercised without hardware;
 * `opened` keeps every native central for staging, `calls` every call.
 */
function realBinding(platform) {
  const addon = loadAddon()
  const profile = DESKTOP_RUST_CORE_PROFILES[platform]
  const bound = bindDesktopCore(
    { platform, operationPrefix: profile.operationPrefix },
    { module: addon, path: addonPath, mode: 'source', sidecar: null }
  )
  const calls = []
  const opened = []
  const productionRequests = []
  // CoreBluetooth waits for its first usable adapter state (legacy, 10 s):
  // the synthetic adapter reports powered-on at open unless a test stages
  // otherwise (`stageFirstState: null` keeps it silent).
  const openSynthetic = async (owner, options) => {
    const central = await bound.openSynthetic(owner, { platform, ...options })
    opened.push(central)
    if (platform === 'corebluetooth' && harness.stageFirstState !== null) {
      await central.stageAdapterState(harness.stageFirstState, true)
    }
    return spyCentral(central, calls)
  }
  const harness = {
    calls,
    opened,
    productionRequests,
    stageFirstState: 'powered-on',
    binding: {
      diagnostics: bound.diagnostics,
      capabilityStates: bound.capabilityStates,
      openSynthetic,
      async openProduction(options) {
        productionRequests.push(options)
        return openSynthetic(options.owner, options.pairingGeneration === true ? { pairingGeneration: true } : undefined)
      },
      async listAdapters() {
        return [{ index: 0, label: 'synthetic-adapter', error: null, displayName: null, default: true }]
      }
    }
  }
  return harness
}

async function openBackend(platform, options = {}) {
  const harness = realBinding(platform)
  const provider = createTestDesktopRustCoreBackendProvider({
    platform,
    owner: options.owner ?? `desktop-test-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: HOST_PLATFORM[platform]
  })
  const adapters = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapters[0].adapterId })
  const stage = harness.opened[harness.opened.length - 1]
  return { backend, stage, harness, provider, adapters }
}

/**
 * Open a backend whose synthetic central is staged before the provider reads
 * its adapter state (`stageBeforeOpen(nativeCentral)`), e.g. power or
 * authorization.
 */
async function openStagedBackend(platform, stageBeforeOpen, options = {}) {
  const harness = realBinding(platform)
  const original = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (owner, openOptions) => {
    const central = await original(owner, openOptions)
    await stageBeforeOpen(harness.opened[harness.opened.length - 1])
    return central
  }
  const provider = createTestDesktopRustCoreBackendProvider({
    platform,
    owner: options.owner ?? `staged-${platform}`,
    now: () => performance.now(),
    radio: options.radio ?? 'synthetic',
    binding: harness.binding,
    hostPlatform: HOST_PLATFORM[platform],
    ...(options.pairingGeneration === undefined ? {} : { pairingGeneration: options.pairingGeneration })
  })
  const adapters = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapters[0].adapterId })
  return { backend, stage: harness.opened[harness.opened.length - 1], harness, provider, adapters }
}

function delivery(itemCapacity = 32) {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(262144),
    reservedControlCapacity: capacity(1024),
    overflowPolicy: 'drop-oldest'
  }
}

function scanOptions(overrides = {}) {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'source-then-receipt',
    delivery: delivery(),
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false },
    ...overrides
  }
}

function withTimeout(promise, timeoutMs, label) {
  let handle
  const timer = new Promise((_, reject) => {
    handle = setTimeout(() => reject(new Error(`${label} did not settle within ${timeoutMs} ms`)), timeoutMs)
  })
  return Promise.race([promise, timer]).finally(() => clearTimeout(handle))
}

/** Next stream item of any kind (value or terminal). */
async function nextItem(iterator, timeoutMs = 3000) {
  const next = await withTimeout(iterator.next(), timeoutMs, 'stream item')
  if (next.done) return { kind: 'done' }
  return next.value
}

async function nextValue(iterator, timeoutMs = 3000) {
  for (;;) {
    const item = await nextItem(iterator, timeoutMs)
    if (item.kind === 'value') return item.value
    throw new Error(`expected a value, got ${JSON.stringify(item)}`)
  }
}

/** Items the stream delivers within `windowMs` (bounded read, no hang). */
async function drainFor(iterator, windowMs) {
  const items = []
  const deadline = Date.now() + windowMs
  for (;;) {
    const remaining = deadline - Date.now()
    if (remaining <= 0) return items
    try {
      const item = await nextItem(iterator, remaining)
      items.push(item)
      if (item.kind !== 'value') return items
    } catch {
      return items
    }
  }
}

/** The next backend event matching `predicate` from one events iterator (the stream is unicast). */
async function nextEvent(iterator, predicate, timeoutMs = 3000) {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const item = await nextItem(iterator, Math.max(1, deadline - Date.now()))
    if (item.kind === 'value' && predicate(item.value)) return item.value
    if (item.kind !== 'value') throw new Error(`event stream ended: ${JSON.stringify(item)}`)
  }
}

function hrmServices({ notify = true, indicate = false } = {}) {
  return [
    {
      uuid: HRM_SERVICE,
      occurrence: 0,
      characteristics: [
        {
          uuid: HRM_MEASUREMENT,
          occurrence: 0,
          properties: { read: true, write: true, writeWithoutResponse: true, notify, indicate },
          descriptors: [{ uuid: USER_DESCRIPTION, occurrence: 0 }]
        },
        {
          uuid: HRM_CONTROL,
          occurrence: 0,
          properties: { read: true, write: true, writeWithoutResponse: false, notify: false, indicate: false },
          descriptors: []
        }
      ]
    }
  ]
}

/** Scan, stage one advertisement, observe it, and return the mapped peer id. */
async function observePeer(backend, stage, advertisement = {}) {
  const lease = await backend.scanner.start(scanOptions(), 'client-1')
  const iterator = lease.observations[Symbol.asyncIterator]()
  try {
    await stage.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Polar H10', ...advertisement })
    const observation = await nextValue(iterator, 5000)
    return observation.device.id
  } finally {
    await iterator.return?.()
    await lease.stop()
  }
}

/** Scan -> connect -> discover over the synthetic radio; returns the pieces. */
async function connectAndDiscover(backend, stage, services = hrmServices()) {
  const peerId = await observePeer(backend, stage)
  await stage.stageMtu('peer-1', 185)
  await stage.stageServices('peer-1', services)
  const lease = await backend.connections.connect(peerId, 'client-1', { signal: null, deadline: null })
  const database = await backend.gatt.discover(lease.connection, { signal: null, deadline: null })
  const snapshot = await database.snapshot()
  const measurement = snapshot.characteristics.find(entry => entry.path.characteristicUuid === HRM_MEASUREMENT)
  const control = snapshot.characteristics.find(entry => entry.path.characteristicUuid === HRM_CONTROL)
  return { peerId, lease, database, snapshot, measurement, control }
}

function subscribeOptions(overrides = {}) {
  return { signal: null, deadline: null, delivery: delivery(), ...overrides }
}

function callNames(calls) {
  return calls.map(([name]) => name)
}

/** Calls that dispatch radio work in Rust (reads of queues/counters excluded). */
const DISPATCH_METHODS = new Set([
  'startScan',
  'stopScan',
  'connect',
  'disconnect',
  'readRssi',
  'discover',
  'read',
  'write',
  'readDescriptor',
  'writeDescriptor',
  'subscribe',
  'unsubscribe',
  'createTicket'
])

function dispatchCalls(calls) {
  return calls.filter(([name]) => DISPATCH_METHODS.has(name))
}

module.exports = {
  HOST_PLATFORM,
  HRM_CONTROL,
  HRM_MEASUREMENT,
  HRM_SERVICE,
  USER_DESCRIPTION,
  addonPath,
  callNames,
  connectAndDiscover,
  delivery,
  dispatchCalls,
  drainFor,
  hrmServices,
  loadAddon,
  nextEvent,
  nextItem,
  nextValue,
  observePeer,
  openBackend,
  openStagedBackend,
  realBinding,
  scanOptions,
  spyCentral,
  subscribeOptions,
  withTimeout
}
