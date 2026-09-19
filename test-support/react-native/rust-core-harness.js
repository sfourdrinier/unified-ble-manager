// test-support/react-native/rust-core-harness.js
//
// Wires the deterministic `UnifiedBleRustCore` native module to the REAL
// production binding (`createReactNativeRustCoreBinding`) and the ordinary
// factories. Nothing below the binding is replaced: arguments are serialized
// by the production codec, envelopes and drains are parsed by it.

const { DeterministicRustCoreNative } = require('./deterministic-rust-core-native')
const { createReactNativeRustCoreBinding } = require('../../src/backends/reactnative/react-native-rust-core-binding')

function rustCoreHarness(options = {}) {
  const platform = options.platform ?? 'android'
  const native = options.native ?? new DeterministicRustCoreNative({ platform, ...(options.nativeOptions ?? {}) })
  const binding = createReactNativeRustCoreBinding({ platform, native })
  return { native, binding, platform }
}

function environment(harness, overrides = {}) {
  return {
    platform: harness.platform,
    now: () => 1000,
    clientId: 'client-a',
    managerId: 'manager-a',
    hostSessionScope: 'scope-a',
    rustCore: harness.binding,
    androidApiLevel: 34,
    ...overrides
  }
}

/** Lets queued wakes and drains settle (microtasks only; no timers). */
async function settle(turns = 20) {
  for (let turn = 0; turn < turns; turn += 1) {
    await Promise.resolve()
  }
}

function scanOptions(overrides = {}) {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: { itemCapacity: 8, byteCapacity: 65536, reservedControlCapacity: 4, overflowPolicy: 'drop-oldest' },
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false },
    ...overrides
  }
}

function subscribeOptions(overrides = {}) {
  return {
    signal: null,
    deadline: null,
    delivery: { itemCapacity: 8, byteCapacity: 4096, reservedControlCapacity: 1, overflowPolicy: 'drop-oldest' },
    ...overrides
  }
}

async function nextValue(stream) {
  const iterator = stream[Symbol.asyncIterator]()
  const result = await iterator.next()
  return result
}

/**
 * The TCK controller hooks into a deterministic module: advertisements,
 * notifications addressed to one exact characteristic instance (complete
 * path, never the UUID alone), and a held pairing ceremony.
 */
function deterministicRustCoreTckBoundary(native) {
  return {
    emitAdvertisement: () => native.emitAdvertisement(),
    emitNotification: (address, bytes) =>
      native.emitNotification(bytes, address.nativePeerId, address.characteristicUuid, {
        serviceUuid: address.serviceUuid,
        serviceOccurrence: address.serviceOccurrence,
        characteristicOccurrence: address.characteristicOccurrence
      }),
    prepareSecurityCancellation: () => native.deferNextPair()
  }
}

module.exports = {
  rustCoreHarness,
  environment,
  settle,
  scanOptions,
  subscribeOptions,
  nextValue,
  deterministicRustCoreTckBoundary
}
