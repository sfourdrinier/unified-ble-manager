const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

const adapterPath = '/org/bluez/hci0'
const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function objects(paired = false) {
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
            Paired: { signature: 'b', value: paired }
          }
        }
      ]
    }
  ]
}

async function createFixture(paired = false) {
  const boundary = new InMemoryBluezBoundary({ objects: objects(paired) })
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
    opaqueId('bluez-security-client', 'client', 'bluez-security')
  )
  const iterator = scan.observations[Symbol.asyncIterator]()
  const observation = iterator.next()
  boundary.queueAdvertisement()
  const item = await observation
  if (item.done || item.value.kind !== 'value') throw new Error('BlueZ security fixture did not observe its peer')
  await iterator.return()
  await scan.stop()
  return { backend, boundary, peerId: String(item.value.value.device.id) }
}

function pairOptions(overrides = {}) {
  return {
    signal: null,
    deadline: null,
    transport: 'auto',
    protection: 'system-default',
    ceremony: 'system',
    ...overrides
  }
}

describe('BlueZ system security backend', () => {
  test('rolls back security watch ownership when the initial state read fails', async () => {
    const { backend } = await createFixture()

    expect(() => backend.security.watch('missing-peer')).toThrow()
    expect(backend.security.streams.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('measures Paired, resolves after the Paired property, watches changes, and removes the OS device', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    const stream = backend.security.watch(observedPeerId)
    const iterator = stream[Symbol.asyncIterator]()

    await expect(backend.security.state(observedPeerId, pairOptions())).resolves.toMatchObject({
      bond: 'not-bonded',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported'
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { sequence: 1, state: { bond: 'not-bonded' } } }
    })
    await expect(backend.security.pair(observedPeerId, pairOptions())).resolves.toMatchObject({
      outcome: 'paired',
      state: { bond: 'bonded' }
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { sequence: 2, state: { bond: 'bonded' } } }
    })
    await expect(backend.security.pair(observedPeerId, pairOptions())).resolves.toMatchObject({
      outcome: 'already-paired'
    })
    await expect(backend.security.unpair(observedPeerId, pairOptions())).resolves.toEqual({ outcome: 'unpaired' })
    expect(boundary.calls.at(-1)).toMatchObject({
      interfaceName: BLUEZ_ADAPTER_INTERFACE,
      method: 'RemoveDevice',
      argumentsValue: [{ signature: 'o', value: devicePath }]
    })
    await iterator.return()
    await stream.close()
    expect(backend.security.streams.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels an in-flight system pairing without claiming a bond', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Pair', () => false)
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await expect(backend.security.cancelPairing(observedPeerId, pairOptions())).resolves.toEqual({
      outcome: 'cancelled'
    })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    await expect(backend.security.state(observedPeerId, pairOptions())).resolves.toMatchObject({ bond: 'not-bonded' })
    await expect(backend.security.cancelPairing(observedPeerId, pairOptions())).resolves.toEqual({
      outcome: 'not-pairing'
    })
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('does not dispatch pairing after the caller has already aborted', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    const controller = new AbortController()
    controller.abort()
    await expect(backend.security.pair(observedPeerId, pairOptions({ signal: controller.signal }))).resolves.toEqual({
      outcome: 'cancelled'
    })
    expect(boundary.calls).not.toEqual(
      expect.arrayContaining([expect.objectContaining({ interfaceName: BLUEZ_DEVICE_INTERFACE, method: 'Pair' })])
    )
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels promptly while the native Pair call remains pending and issues one CancelPairing', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    await expect(backend.security.cancelPairing(observedPeerId, pairOptions())).resolves.toEqual({
      outcome: 'cancelled'
    })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
    ).toHaveLength(1)

    resolvePair()
    await Promise.resolve()
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels promptly at a deadline while the native Pair call remains pending', async () => {
    jest.useFakeTimers()
    try {
      const { backend, boundary, peerId: observedPeerId } = await createFixture()
      let resolvePair = () => undefined
      boundary.onCall(
        devicePath,
        BLUEZ_DEVICE_INTERFACE,
        'Pair',
        () =>
          new Promise(resolve => {
            resolvePair = resolve
          })
      )
      const pairing = backend.security.pair(observedPeerId, pairOptions({ deadline: 110 }))
      const result = expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
      await jest.advanceTimersByTimeAsync(10)

      await result
      expect(
        boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
      ).toHaveLength(1)
      resolvePair()
      await Promise.resolve()
      await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
    } finally {
      jest.useRealTimers()
    }
  })

  test('cancels active pairing and terminates security watchers when the device is removed', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const stream = backend.security.watch(observedPeerId)
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const terminal = iterator.next()
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    boundary.objectManager.emitInterfacesRemoved(devicePath, [BLUEZ_DEVICE_INTERFACE])
    await expect(terminal).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'operation-aborted' } })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
    ).toHaveLength(1)

    const active = backend.security.activePairings.get(observedPeerId)
    if (active === undefined) throw new Error('BlueZ active pairing disappeared before physical settlement')
    resolvePair()
    await active.dispatch.physicalSettled
    expect(backend.security.activePairings.size).toBe(0)
    expect(backend.security.streams.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('cancels active pairing and terminates security watchers on a BlueZ daemon reset', async () => {
    const { backend, boundary, peerId: observedPeerId } = await createFixture()
    let resolvePair = () => undefined
    boundary.onCall(
      devicePath,
      BLUEZ_DEVICE_INTERFACE,
      'Pair',
      () =>
        new Promise(resolve => {
          resolvePair = resolve
        })
    )
    const stream = backend.security.watch(observedPeerId)
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const terminal = iterator.next()
    const pairing = backend.security.pair(observedPeerId, pairOptions())
    await Promise.resolve()
    await Promise.resolve()

    boundary.emitReset('test reset')
    await expect(terminal).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'source-failed' } })
    await expect(pairing).resolves.toEqual({ outcome: 'cancelled' })
    expect(
      boundary.calls.filter(call => call.interfaceName === BLUEZ_DEVICE_INTERFACE && call.method === 'CancelPairing')
    ).toHaveLength(1)

    const active = backend.security.activePairings.get(observedPeerId)
    if (active === undefined) throw new Error('BlueZ active pairing disappeared before physical settlement')
    resolvePair()
    await active.dispatch.physicalSettled
    expect(backend.security.activePairings.size).toBe(0)
    await expect(backend.destroy()).resolves.toMatchObject({ state: 'released' })
  })
})
