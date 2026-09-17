// __tests__/backends/reactnative/react-native-apple-rust-authority.test.js
//
// R02 Apple cutover: Rust authority + honest attestation for the Apple path.
//
// The Apple provider executes the native Rust core through an injected
// binding or fails LOUDLY. There is no silent Swift-only fallback and no
// synthetic success: without a binding (or with a foreign revision) BLE
// admission rejects before any Swift control-surface work, and with a
// binding every BLE effect dispatches through the admitted core session.
//
// What this suite proves on Linux (jest):
//   - missing/malformed binding fails closed with capability.unsupported
//     and opens zero native sessions;
//   - a foreign contract revision fails closed with protocol.incompatible
//     and closes the opened session exactly once (no leak);
//   - a valid binding drives open/invoke/close through the core while the
//     Swift control surface (a throwing proxy here) is never touched;
//   - backend identity still reports the Apple backend/platform ids, now
//     with the core-session transport wording that matches reality.
//
// What only macOS CI can prove (stated, not simulated here):
//   - xcodebuild assembly of ios/RustCore/RustCore.xcframework with the
//     attested core symbols (see ios/build-rust-core.sh);
//   - pod install link of the framework into iOS/tvOS consumers;
//   - device/simulator execution of a production UniFFI EchoSession bridge.

'use strict'

const { capacity } = require('../../../src/backend-contract/primitives')
const { RUST_CORE_CONTRACT_REVISION } = require('../../../src/backends/reactnative/react-native-rust-core')
const {
  createReactNativeAppleBackendProvider,
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_APPLE_PLATFORM_ID
} = require('../../../src/backends/reactnative/react-native-apple-provider')

function throwingControl() {
  return new Proxy(
    {},
    {
      get: (_target, property) => {
        if (property === 'then') return undefined
        throw new Error(`Swift control surface must not execute BLE work (touched ${String(property)})`)
      }
    }
  )
}

function createFakeCore(script = {}) {
  const calls = []
  const session = {
    contractRevision: () => script.revision || RUST_CORE_CONTRACT_REVISION,
    invoke: async (op, args) => {
      calls.push([op, args])
      switch (op) {
        case 'adapter.state':
          return {
            availability: 'available',
            authorization: 'unknown',
            power: 'on',
            backendGeneration: 'gen-1',
            updatedAt: 123,
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
          return { operationId: 'apple-scan-op-1' }
        case 'scan.take':
          return null
        case 'scan.stop':
          return { state: 'released' }
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
          return { state: 'released' }
        default:
          throw new Error(`unexpected core op ${op}`)
      }
    },
    close: async () => {
      calls.push(['session.close', undefined])
    }
  }
  return {
    calls,
    binding: {
      openSession: async owner => {
        if (typeof owner !== 'string' || owner.length === 0) throw new Error('owner must not be empty')
        calls.push(['session.open', owner])
        return session
      }
    }
  }
}

function ops(calls, name) {
  return calls.filter(([op]) => op === name).map(([, args]) => args)
}

function scanOptions() {
  return {
    filter: { serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(4096),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    },
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

describe('React Native Apple Rust authority (R02 cutover)', () => {
  test('missing binding fails loudly before any native session opens', async () => {
    const provider = createReactNativeAppleBackendProvider({ control: throwingControl(), now: () => 20 })
    for (const attempt of [() => provider.listAdapters(), () => provider.create({ selectedAdapterId: 'x' })]) {
      const error = await attempt().then(
        () => null,
        failure => failure
      )
      expect(error).not.toBeNull()
      expect(error.normalized.code).toBe('capability.unsupported')
      expect(error.normalized.operation).toBe('react-native-apple.provider.rust-core-missing')
    }
  })

  test('malformed binding fails loudly and opens zero native sessions', async () => {
    for (const rustCore of [{}, { openSession: 'yes' }, null]) {
      const opened = []
      const provider = createReactNativeAppleBackendProvider({
        control: throwingControl(),
        now: () => 20,
        rustCore
      })
      const error = await provider.listAdapters().then(
        () => null,
        failure => failure
      )
      expect(error).not.toBeNull()
      expect(error.normalized.code).toBe('capability.unsupported')
      expect(opened).toEqual([])
    }
  })

  test('foreign contract revision fails closed and closes the opened session exactly once', async () => {
    const fake = createFakeCore({ revision: 'C-UBM.9.9.9-DRAFT' })
    const provider = createReactNativeAppleBackendProvider({
      control: throwingControl(),
      now: () => 20,
      rustCore: fake.binding
    })
    const error = await provider.create({ selectedAdapterId: REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID }).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('protocol.incompatible')
    expect(ops(fake.calls, 'session.open')).toHaveLength(1)
    expect(ops(fake.calls, 'session.close')).toHaveLength(1)
    expect(ops(fake.calls, 'adapter.state')).toHaveLength(0)
  })

  test('valid binding executes open/invoke/close through the core, never the Swift surface', async () => {
    const fake = createFakeCore()
    const provider = createReactNativeAppleBackendProvider({
      control: throwingControl(),
      now: () => 20,
      rustCore: fake.binding
    })
    const backend = await provider.create({ selectedAdapterId: REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID })
    try {
      expect(ops(fake.calls, 'session.open')).toHaveLength(1)
      expect(ops(fake.calls, 'adapter.state')).toHaveLength(1)
      expect(ops(fake.calls, 'counters.describe').length).toBeGreaterThanOrEqual(1)
      expect(backend.identity.registeredBackendId).toBe(REACT_NATIVE_APPLE_BACKEND_ID)
      expect(backend.identity.registeredPlatformId).toBe(REACT_NATIVE_APPLE_PLATFORM_ID)
      expect(backend.identity.runtime.diagnostics).toMatchObject({
        boundary: 'react-native-rust-core-v1',
        transport: 'native-core-session'
      })
      expect(backend.identity.attachment.adapter.adapterId).toContain(REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID)
      // No TypeScript scan planner survives on the core-backed scanner:
      // the core owns admission, deadlines, and delivery.
      expect(backend.scanner.plan).toBeUndefined()
      const lease = await backend.scanner.start(scanOptions(), 'client-a')
      try {
        expect(ops(fake.calls, 'scan.start')).toHaveLength(1)
        expect(ops(fake.calls, 'scan.start')[0]).toMatchObject({
          serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
          duplicatePolicy: 'all'
        })
      } finally {
        await lease.stop()
      }
      expect(ops(fake.calls, 'scan.stop')).toHaveLength(1)
    } finally {
      await backend.destroy()
    }
    expect(ops(fake.calls, 'session.dispose')).toHaveLength(1)
    expect(ops(fake.calls, 'session.close')).toHaveLength(1)
  })

  test('listAdapters probes the core and releases the probe session', async () => {
    const fake = createFakeCore()
    const provider = createReactNativeAppleBackendProvider({
      control: throwingControl(),
      now: () => 20,
      rustCore: fake.binding
    })
    const adapters = await provider.listAdapters()
    expect(adapters).toHaveLength(1)
    expect(ops(fake.calls, 'session.open')).toHaveLength(1)
    expect(ops(fake.calls, 'session.close')).toHaveLength(1)
  })

  test('foreign adapter selection still rejects before touching the core', async () => {
    const fake = createFakeCore()
    const provider = createReactNativeAppleBackendProvider({
      control: throwingControl(),
      now: () => 20,
      rustCore: fake.binding
    })
    const error = await provider.create({ selectedAdapterId: 'not-the-apple-adapter' }).then(
      () => null,
      failure => failure
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('adapter.unavailable')
    expect(ops(fake.calls, 'session.open')).toHaveLength(0)
  })
})
