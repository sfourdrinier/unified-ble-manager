const { ReactNativeAndroidSecurityBackend } = require('../../src/backends/reactnative/react-native-android-security')
const { inspectAndroidSecurityStreamOwnershipForTests } = require('../../src/backends/reactnative/react-native-android-security')
const { WinRtSecurityBackend } = require('../../src/backends/winrt/winrt-security')
const { inspectWinRtSecurityStreamOwnershipForTests } = require('../../src/backends/winrt/winrt-security')
const { inspectDeterministicSecurityStreamOwnershipForTests } = require('../../src/testing/deterministic/deterministic-security')
const { inspectBluezSecurityStreamOwnershipForTests } = require('../../src/backends/bluez/bluez-security')
const { createBluezBackendProvider } = require('../../src/backends/bluez/bluez-backend-provider')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { opaqueId } = require('../../src/backend-contract/primitives')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../test-support/bluez/in-memory-bluez-object-manager')

function securityState(bond = 'not-bonded') {
  return {
    bond,
    encryption: 'unsupported',
    authentication: 'unsupported',
    secureConnections: 'unsupported',
    pairingPossible: true
  }
}

function pairOptions() {
  return {
    signal: null,
    deadline: null,
    transport: 'auto',
    protection: 'system-default',
    ceremony: 'system'
  }
}

function spyDelivery(stream) {
  if (typeof stream.emit === 'function') {
    return jest.spyOn(stream, 'emit')
  }
  if (typeof stream.push === 'function') {
    return jest.spyOn(stream, 'push')
  }
  return null
}

function createAndroidHarness() {
  const listeners = new Set()
  const security = new ReactNativeAndroidSecurityBackend(
    {
      securityState: jest.fn(async () => securityState()),
      pair: jest.fn(async () => ({ outcome: 'paired', state: securityState('bonded') })),
      cancelPairing: jest.fn(async () => undefined),
      unpair: jest.fn(async () => 'unsupported'),
      onSecurityState: listener => {
        listeners.add(listener)
        return () => listeners.delete(listener)
      },
      securityAvailable: true,
      securityCancellationAvailable: true
    },
    () => 20
  )
  return {
    security,
    inspect: () => inspectAndroidSecurityStreamOwnershipForTests(security),
    emitLater: () => {
      for (const listener of listeners) {
        listener({ nativePeerId: 'peer-1', state: securityState('bonded') })
      }
    },
    closeBackend: () => security.close(),
    resetBackend: () => security.close()
  }
}

function createWinRtHarness() {
  const listeners = new Set()
  const boundary = {
    securityState: jest.fn(() => ({
      completion: Promise.resolve(securityState()),
      cancel: jest.fn(async () => 'already-terminal')
    })),
    pair: jest.fn(() => ({
      completion: Promise.resolve({ outcome: 'paired', state: securityState('bonded'), reason: null }),
      cancel: jest.fn(async () => 'already-terminal')
    })),
    cancelPairing: jest.fn(() => ({
      completion: Promise.resolve(),
      cancel: jest.fn(async () => 'already-terminal')
    })),
    unpair: jest.fn(() => ({
      completion: Promise.resolve('unpaired'),
      cancel: jest.fn(async () => 'already-terminal')
    })),
    onSecurityState: listener => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    }
  }
  const security = new WinRtSecurityBackend(boundary, () => 50)
  return {
    security,
    inspect: () => inspectWinRtSecurityStreamOwnershipForTests(security),
    emitLater: () => {
      for (const listener of listeners) {
        listener({ nativePeerId: 'peer-1', state: securityState('bonded') })
      }
    },
    closeBackend: () => security.close(),
    resetBackend: () => security.resetForAdapterLoss()
  }
}

function createDeterministicHarness() {
  const fixture = createDeterministicTestBackend()
  const security = fixture.backend.security
  return {
    security,
    fixture,
    inspect: () => inspectDeterministicSecurityStreamOwnershipForTests(security),
    emitLater: async () => {
      const pair = security.pair('peer-1', pairOptions())
      fixture.controller.clock.runUntilIdle()
      await pair
    },
    closeBackend: () => security.close(),
    resetBackend: () => security.close()
  }
}

const adapterPath = '/org/bluez/hci0'
const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`

function bluezObjects() {
  return [
    {
      path: adapterPath,
      interfaces: [
        {
          name: BLUEZ_ADAPTER_INTERFACE,
          properties: {
            Address: { signature: 's', value: '00:11:22:33:44:55' },
            Alias: { signature: 's', value: 'primary' },
            Powered: { signature: 'b', value: true }
          }
        }
      ]
    },
    {
      path: devicePath,
      interfaces: [
        {
          name: BLUEZ_DEVICE_INTERFACE,
          properties: {
            Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
            Alias: { signature: 's', value: 'security peer' },
            RSSI: { signature: 'n', value: -40 },
            UUIDs: { signature: 'as', value: [] },
            Connected: { signature: 'b', value: false },
            ServicesResolved: { signature: 'b', value: false },
            Paired: { signature: 'b', value: false }
          }
        }
      ]
    }
  ]
}

async function createBluezHarness() {
  const boundary = new InMemoryBluezBoundary({ objects: bluezObjects() })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 100
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  const scan = await backend.scanner.start(
    {
      filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
      duplicatePolicy: 'all',
      timestampPolicy: 'receipt-monotonic',
      delivery: { itemCapacity: 4, byteCapacity: 4096, reservedControlCapacity: 1, overflowPolicy: 'drop-oldest' },
      deadline: null,
      signal: null,
      sharing: { mode: 'owner', allowSharing: false }
    },
    opaqueId('bluez-security-unregister-client', 'client', 'bluez-security')
  )
  const iterator = scan.observations[Symbol.asyncIterator]()
  const observation = iterator.next()
  boundary.queueAdvertisement()
  const item = await observation
  if (item.done || item.value.kind !== 'value') {
    throw new Error('BlueZ security fixture did not observe its peer')
  }
  await iterator.return()
  await scan.stop()
  const peerId = String(item.value.value.device.id)
  return {
    security: backend.security,
    peerId,
    inspect: () => inspectBluezSecurityStreamOwnershipForTests(backend.security),
    emitLater: () => {
      backend.security.propertiesChanged({
        path: devicePath,
        interfaceName: BLUEZ_DEVICE_INTERFACE,
        changed: { Paired: { signature: 'b', value: true } }
      })
    },
    closeBackend: () => backend.security.close(),
    resetBackend: () => backend.security.reset()
  }
}

async function flush() {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

describe('security watch unregister', () => {
  test('android security close removes the stream from the registry', async () => {
    const harness = createAndroidHarness()
    const stream = harness.security.watch('peer-1')
    expect(harness.inspect()).toEqual({ peerCount: 1, streamCount: 1 })
    await stream.close()
    expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
  })

  test('winrt security close removes the stream from the registry', async () => {
    const harness = createWinRtHarness()
    const stream = harness.security.watch('peer-1')
    expect(harness.inspect()).toEqual({ peerCount: 1, streamCount: 1 })
    await stream.close()
    expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
  })

  test('deterministic security close removes the stream from the registry', async () => {
    const harness = createDeterministicHarness()
    const stream = harness.security.watch('peer-1')
    expect(harness.inspect()).toEqual({ peerCount: 1, streamCount: 1 })
    await stream.close()
    expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
  })

  test('bluez security close already unregisters (control)', async () => {
    const harness = await createBluezHarness()
    const stream = harness.security.watch(harness.peerId)
    expect(harness.inspect()).toEqual({ peerCount: 1, streamCount: 1 })
    await stream.close()
    expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
  })

  test('later security event does not iterate a closed stream', async () => {
    const android = createAndroidHarness()
    const androidStream = android.security.watch('peer-1')
    const androidSpy = spyDelivery(androidStream)
    await androidStream.close()
    androidSpy.mockClear()
    android.emitLater()
    expect(androidSpy).not.toHaveBeenCalled()

    const winrt = createWinRtHarness()
    const winrtStream = winrt.security.watch('peer-1')
    const winrtSpy = spyDelivery(winrtStream)
    await winrtStream.close()
    winrtSpy.mockClear()
    winrt.emitLater()
    expect(winrtSpy).not.toHaveBeenCalled()

    const deterministic = createDeterministicHarness()
    const deterministicStream = deterministic.security.watch('peer-1')
    const deterministicSpy = spyDelivery(deterministicStream)
    await deterministicStream.close()
    deterministicSpy.mockClear()
    await deterministic.emitLater()
    expect(deterministicSpy).not.toHaveBeenCalled()

    const bluez = await createBluezHarness()
    const bluezStream = bluez.security.watch(bluez.peerId)
    await bluezStream.close()
    expect(bluez.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
    bluez.emitLater()
    expect(bluez.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
  })

  test('repeated open/close does not grow the map', async () => {
    const harnesses = [createAndroidHarness(), createWinRtHarness(), createDeterministicHarness(), await createBluezHarness()]
    for (const harness of harnesses) {
      const peerId = harness.peerId ?? 'peer-1'
      for (let index = 0; index < 8; index += 1) {
        const stream = harness.security.watch(peerId)
        await stream.close()
      }
      expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
    }
  })

  test('close terminal reset and destroy race unregisters exactly once', async () => {
    const harnesses = [createAndroidHarness(), createWinRtHarness(), createDeterministicHarness(), await createBluezHarness()]
    for (const harness of harnesses) {
      const peerId = harness.peerId ?? 'peer-1'
      const stream = harness.security.watch(peerId)
      await Promise.all([
        stream.close(),
        Promise.resolve().then(() => {
          if (typeof stream.closeWithReason === 'function') {
            stream.closeWithReason('overflow')
          }
        }),
        Promise.resolve().then(() => harness.resetBackend())
      ])
      expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
    }
  })

  test('deterministic retained and reserved stream bytes return to zero', async () => {
    const harness = createDeterministicHarness()
    const before = harness.security.reservedBytes()
    const beforeCounters = harness.fixture.backend.resourceCounters()
    const stream = harness.security.watch('peer-1')
    expect(harness.security.reservedBytes()).toBeGreaterThan(before)
    await stream.close()
    expect(harness.inspect()).toEqual({ peerCount: 0, streamCount: 0 })
    expect(harness.security.reservedBytes()).toBe(before)
    expect(harness.fixture.backend.resourceCounters().retainedByteBuffers).toBe(beforeCounters.retainedByteBuffers)
    await flush()
  })
})
