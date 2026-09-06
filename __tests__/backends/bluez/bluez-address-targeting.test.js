// __tests__/backends/bluez/bluez-address-targeting.test.js
//
// peer:address-targeting on BlueZ: connect to a peer by an out-of-band radio
// address with no prior scan, with pending semantics — the peer does not need
// to be advertising at call time. Bootstrap prefers Adapter1.ConnectDevice and
// falls back to an address-filtered bootstrap discovery on daemons without it.

const { attachBackend } = require('../../../src/backend-contract/backend')
const { opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const { BluezDbusMethodError } = require('../../../src/backends/bluez/bluez-dbus-contract')
const { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } = require('../../../src/manager/ble-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../../src/public/scan-query')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

const adapterPath = '/org/bluez/hci0'
const address = '98:75:96:A2:14:34'
const devicePath = `${adapterPath}/dev_98_75_96_A2_14_34`

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function operation(signal = null) {
  return { signal, deadline: null }
}

function adapterObject() {
  return {
    path: adapterPath,
    interfaces: [
      {
        name: BLUEZ_ADAPTER_INTERFACE,
        properties: {
          Address: { signature: 's', value: '00:11:22:33:44:55' },
          Alias: { signature: 's', value: 'primary' },
          Powered: { signature: 'b', value: true },
          Discovering: { signature: 'b', value: false }
        }
      }
    ]
  }
}

function deviceInterface(connected = false) {
  return {
    name: BLUEZ_DEVICE_INTERFACE,
    properties: {
      Address: { signature: 's', value: address },
      AddressType: { signature: 's', value: 'public' },
      Alias: { signature: 's', value: address },
      RSSI: { signature: 'n', value: -70 },
      Connected: { signature: 'b', value: connected },
      ServicesResolved: { signature: 'b', value: false }
    }
  }
}

function deviceObject(connected = false) {
  return { path: devicePath, interfaces: [deviceInterface(connected)] }
}

async function backendFixture(objects) {
  const boundary = new InMemoryBluezBoundary({ objects })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => Date.now()
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  await attachBackend(backend, compatibility())
  return { backend, boundary }
}

function unknownMethodError() {
  return new BluezDbusMethodError({
    name: 'org.freedesktop.DBus.Error.UnknownMethod',
    message: 'Unknown method ConnectDevice',
    safeDetails: {}
  })
}

function establishmentFailure() {
  return new BluezDbusMethodError({
    name: 'org.bluez.Error.Failed',
    message: 'le-connection-abort-by-local',
    safeDetails: {}
  })
}

const clientId = opaqueId('address-client', 'client', 'bluez:test')

describe('BlueZ peer:address-targeting', () => {
  test('registers the peer:address-targeting capability', async () => {
    const { backend } = await backendFixture([adapterObject()])
    const descriptor = backend.features.descriptors.find(entry => entry.id === 'peer:address-targeting')
    expect(descriptor).toMatchObject({ state: 'limited', implementationOrigin: 'backend-native' })
    await backend.destroy()
  })

  test('rejects a non-canonical or malformed backend address descriptor', async () => {
    const { backend } = await backendFixture([adapterObject()])
    for (const bad of ['aa:bb:cc:dd:ee:ff', '98-75-96-A2-14-34', 'nope', '']) {
      expect(() => backend.connections.peerFromAddress({ address: bad, addressType: 'public' })).toThrow(
        expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) })
      )
    }
    expect(() => backend.connections.peerFromAddress({ address, addressType: 'opaque' })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) })
    )
    await backend.destroy()
  })

  test('connects by address without a prior scan when the device object already exists', async () => {
    const { backend, boundary } = await backendFixture([adapterObject(), deviceObject()])
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const lease = await backend.connections.connect(peerId, clientId, operation())
    expect(String(lease.connection.peerId)).toBe(String(peerId))
    expect(boundary.calls.filter(call => call.method === 'Connect')).toHaveLength(1)
    // No scan or ConnectDevice was needed: the known device object was used directly.
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(0)
    expect(boundary.calls.filter(call => call.method === 'ConnectDevice')).toHaveLength(0)
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await backend.destroy()
  })

  test('stays pending across failed establishment attempts until the peer answers', async () => {
    const { backend, boundary } = await backendFixture([adapterObject(), deviceObject()])
    let attempts = 0
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      attempts += 1
      if (attempts < 3) {
        throw establishmentFailure()
      }
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const lease = await backend.connections.connect(peerId, clientId, operation())
    expect(attempts).toBe(3)
    for (let failure = 0; failure < 2; failure += 1) {
      expectConsoleErrorMatching(
        '[connectBluezConnection] Shared BlueZ connect transition failed:',
        expect.objectContaining({ normalized: expect.objectContaining({ code: 'platform.failure' }) })
      )
    }
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await backend.destroy()
  })

  test('bootstraps a never-seen address through Adapter1.ConnectDevice when available', async () => {
    const { backend, boundary } = await backendFixture([adapterObject()])
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', async call => {
      const properties = call.argumentsValue[0].value
      expect(properties.Address.value).toBe(address)
      expect(properties.AddressType.value).toBe('public')
      boundary.objectManager.objects.push(deviceObject(true))
      boundary.objectManager.emitInterfacesAdded(devicePath, [deviceInterface(true)])
    })
    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const lease = await backend.connections.connect(peerId, clientId, operation())
    expect(boundary.calls.filter(call => call.method === 'ConnectDevice')).toHaveLength(1)
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(0)
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await backend.destroy()
  })

  test('falls back to an address-filtered bootstrap discovery on daemons without ConnectDevice', async () => {
    const { backend, boundary } = await backendFixture([adapterObject()])
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', async () => {
      throw unknownMethodError()
    })
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StartDiscovery', async () => {
      // The peer's advertising burst arrives while the bootstrap discovery listens.
      setImmediate(() => {
        boundary.objectManager.objects.push(deviceObject())
        boundary.objectManager.emitInterfacesAdded(devicePath, [deviceInterface()])
      })
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const lease = await backend.connections.connect(peerId, clientId, operation())
    const discoveryFilter = boundary.calls.find(call => call.method === 'SetDiscoveryFilter')
    expect(discoveryFilter.argumentsValue[0].value.Pattern).toEqual({ signature: 's', value: address })
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(1)
    expect(boundary.calls.filter(call => call.method === 'StopDiscovery')).toHaveLength(1)
    expect(boundary.calls.filter(call => call.method === 'Connect')).toHaveLength(1)
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await backend.destroy()
  })

  test('aborts a pending address connect cleanly while the bootstrap discovery is waiting', async () => {
    const { backend, boundary } = await backendFixture([adapterObject()])
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', async () => {
      throw unknownMethodError()
    })
    const abortController = new AbortController()
    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const pending = backend.connections.connect(peerId, clientId, operation(abortController.signal))
    await new Promise(resolve => setImmediate(resolve))
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(1)
    abortController.abort()
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await new Promise(resolve => setImmediate(resolve))
    expect(boundary.calls.filter(call => call.method === 'StopDiscovery')).toHaveLength(1)
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(0)
    await backend.destroy()
  })

  test('rejects an already-expired deadline without touching the radio', async () => {
    const { backend, boundary } = await backendFixture([adapterObject()])
    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    await expect(
      backend.connections.connect(peerId, clientId, { signal: null, deadline: Date.now() - 1 })
    ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    expect(boundary.calls).toHaveLength(0)
    await backend.destroy()
  })

  test('exposes the radio address on scan observations so addresses clauses match', async () => {
    const { backend, boundary } = await backendFixture([adapterObject(), deviceObject()])
    const scan = await backend.scanner.start(
      {
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
        duplicatePolicy: 'all',
        timestampPolicy: 'receipt-monotonic',
        delivery: {
          itemCapacity: 4,
          byteCapacity: 4096,
          reservedControlCapacity: 1,
          overflowPolicy: 'drop-oldest'
        },
        deadline: null,
        signal: null,
        sharing: { mode: 'owner', allowSharing: false }
      },
      opaqueId('address-observer', 'client', 'bluez:test')
    )
    boundary.queueAdvertisement()
    const item = await scan.observations[Symbol.asyncIterator]().next()
    await scan.stop()
    expect(item.value.kind).toBe('value')
    const normalized = normalizeScanObservation(item.value.value)
    expect(normalized.address).toEqual({ type: 'public', value: address })
    const query = normalizeScanQuery({ anyOf: [{ addresses: [address.toLowerCase()] }] })
    expect(observationMatchesScanQuery(query, normalized)).toBe(true)
    await backend.destroy()
  })

  test('connects by address through the public manager end to end', async () => {
    const boundary = new InMemoryBluezBoundary({ objects: [adapterObject(), deviceObject()] })
    const provider = createBluezBackendProvider({
      busKind: 'system',
      boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
      now: () => Date.now()
    })
    const internal = await createBleManagerFromProvider(
      {
        provider,
        selection: { selectedAdapterId: adapterPath },
        coreCompatibility: compatibility(),
        manager: {
          clientId: opaqueId('bluez-address-client', 'client', 'bluez:manager'),
          managerId: opaqueId('bluez-address-manager', 'manager', 'bluez:manager'),
          ownerMode: 'owning'
        }
      },
      DEFAULT_BLE_MANAGER_OPTIONS
    )
    const manager = await createPublicBleManager(internal, () => Date.now())
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)
    // Accepts a non-canonical spelling; the public layer canonicalizes it.
    const connection = await manager.connect({ address: '98-75-96-a2-14-34' })
    expect(connection.peer.id.length).toBeGreaterThan(0)
    expect(boundary.calls.filter(call => call.method === 'Connect')).toHaveLength(1)
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(0)
    await connection.release()
    await manager.destroy()
  })

  test('bounds a hung ConnectDevice by the caller deadline and does not start a second call', async () => {
    jest.useFakeTimers({ now: 1_000 })
    let backend
    try {
      const fixtureResult = await backendFixture([adapterObject()])
      backend = fixtureResult.backend
      const { boundary } = fixtureResult
      let releaseConnectDevice
      const connectDeviceGate = new Promise(resolve => {
        releaseConnectDevice = resolve
      })
      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', async () => {
        await connectDeviceGate
        boundary.objectManager.objects.push(deviceObject(true))
        boundary.objectManager.emitInterfacesAdded(devicePath, [deviceInterface(true)])
      })

      const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
      let firstOutcome = 'pending'
      const first = backend.connections.connect(peerId, clientId, { signal: null, deadline: Date.now() + 20 })
      first.then(
        () => {
          firstOutcome = 'resolved'
        },
        error => {
          firstOutcome = error.normalized?.code ?? 'rejected'
        }
      )
      await jest.advanceTimersByTimeAsync(0)
      expect(boundary.calls.filter(call => call.method === 'ConnectDevice')).toHaveLength(1)
      await jest.advanceTimersByTimeAsync(20)
      expect(firstOutcome).toBe('operation.timed-out')
      await expect(first).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })

      let secondOutcome = 'pending'
      const second = backend.connections.connect(peerId, clientId, { signal: null, deadline: Date.now() + 20 })
      second.then(
        () => {
          secondOutcome = 'resolved'
        },
        error => {
          secondOutcome = error.normalized?.code ?? 'rejected'
        }
      )
      await jest.advanceTimersByTimeAsync(20)
      expect(secondOutcome).toBe('operation.timed-out')
      await expect(second).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
      expect(boundary.calls.filter(call => call.method === 'ConnectDevice')).toHaveLength(1)

      releaseConnectDevice()
      await jest.advanceTimersByTimeAsync(0)
      await Promise.resolve()
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      jest.useRealTimers()
      if (backend !== undefined) {
        await backend.destroy().catch(() => undefined)
      }
    }
  })

  test('widens an excluding live scan so address fallback can observe the target', async () => {
    const heartRateUuid = '0000180d-0000-1000-8000-00805f9b34fb'
    const polarPath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
    const polarInterface = {
      name: BLUEZ_DEVICE_INTERFACE,
      properties: {
        Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
        AddressType: { signature: 's', value: 'random' },
        Alias: { signature: 's', value: 'Polar H10' },
        RSSI: { signature: 'n', value: -48 },
        UUIDs: { signature: 'as', value: [heartRateUuid] },
        Connected: { signature: 'b', value: false },
        ServicesResolved: { signature: 'b', value: false }
      }
    }
    const { backend, boundary } = await backendFixture([
      adapterObject(),
      { path: polarPath, interfaces: [polarInterface] }
    ])
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', async () => {
      throw unknownMethodError()
    })
    const scan = await backend.scanner.start(
      {
        filter: { serviceUuids: [heartRateUuid], manufacturerData: [], localNamePrefix: 'Polar' },
        duplicatePolicy: 'all',
        timestampPolicy: 'receipt-monotonic',
        delivery: {
          itemCapacity: 4,
          byteCapacity: 4096,
          reservedControlCapacity: 1,
          overflowPolicy: 'drop-oldest'
        },
        deadline: null,
        signal: null,
        sharing: { mode: 'owner', allowSharing: false }
      },
      opaqueId('excluding-scan', 'client', 'bluez:test')
    )
    const seenNames = []
    const iterator = scan.observations[Symbol.asyncIterator]()
    const firstObservation = await iterator.next()
    expect(firstObservation.value.value.localName.value).toBe('Polar H10')
    seenNames.push(firstObservation.value.value.localName.value)

    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'SetDiscoveryFilter', async call => {
      const filter = call.argumentsValue[0].value
      if (filter.UUIDs === undefined) {
        setImmediate(() => {
          if (!boundary.objectManager.objects.some(candidate => candidate.path === devicePath)) {
            boundary.objectManager.objects.push(deviceObject())
            boundary.objectManager.emitInterfacesAdded(devicePath, [deviceInterface()])
          }
        })
      }
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)

    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const lease = await backend.connections.connect(peerId, clientId, {
      signal: null,
      deadline: Date.now() + 500
    })
    const widened = boundary.calls.filter(
      call => call.method === 'SetDiscoveryFilter' && call.argumentsValue[0].value.UUIDs === undefined
    )
    expect(widened.length).toBeGreaterThanOrEqual(1)
    const restored = boundary.calls.filter(
      call =>
        call.method === 'SetDiscoveryFilter' &&
        Array.isArray(call.argumentsValue[0].value.UUIDs?.value) &&
        call.argumentsValue[0].value.UUIDs.value.includes(heartRateUuid)
    )
    expect(restored.length).toBeGreaterThanOrEqual(2)
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(1)

    const nextObservation = iterator.next()
    let extraObservation = null
    nextObservation.then(item => {
      extraObservation = item
    })
    await new Promise(resolve => setImmediate(resolve))
    expect(extraObservation).toBe(null)

    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(seenNames).toEqual(['Polar H10'])
    await backend.destroy()
  })

  test('reuses a compatible live scan for address fallback without a second StartDiscovery', async () => {
    const { backend, boundary } = await backendFixture([adapterObject()])
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'ConnectDevice', async () => {
      throw unknownMethodError()
    })
    const scan = await backend.scanner.start(
      {
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
        duplicatePolicy: 'all',
        timestampPolicy: 'receipt-monotonic',
        delivery: {
          itemCapacity: 4,
          byteCapacity: 4096,
          reservedControlCapacity: 1,
          overflowPolicy: 'drop-oldest'
        },
        deadline: null,
        signal: null,
        sharing: { mode: 'owner', allowSharing: false }
      },
      opaqueId('compatible-scan', 'client', 'bluez:test')
    )
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)

    const peerId = backend.connections.peerFromAddress({ address, addressType: 'public' })
    const pending = backend.connections.connect(peerId, clientId, operation())
    await new Promise(resolve => setImmediate(resolve))
    boundary.objectManager.objects.push(deviceObject())
    boundary.objectManager.emitInterfacesAdded(devicePath, [deviceInterface()])
    const lease = await pending
    expect(boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(1)
    expect(
      boundary.calls.filter(
        call => call.method === 'SetDiscoveryFilter' && call.argumentsValue[0].value.Pattern?.value === address
      )
    ).toHaveLength(0)
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    await backend.destroy()
  })
})
