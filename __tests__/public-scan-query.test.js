const {
  normalizeScanQuery,
  normalizeScanObservation,
  observationMatchesScanQuery
} = require('../src/public/scan-query')
const {
  createPublicBleManager,
  filterScanObservations,
  findPeerInScan,
  inspectPublicScanFingerprintAccountingForTests
} = require('../src/public/ble-manager')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
const { createDeterministicTestBleManager } = require('../src/testing/deterministic/deterministic-test-manager')
const { deterministicScenarioAdvertisement } = require('../src/testing/scenarios/manager-scenario-executor')

function observation(overrides = {}) {
  return {
    localName: 'Heart Strap',
    rssi: -42,
    connectable: true,
    serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb', '0000180f-0000-1000-8000-00805f9b34fb'],
    manufacturerData: [
      { companyId: 76, data: new Uint8Array([2, 21, 1, 2]) },
      { companyId: 123, data: new Uint8Array([9, 8]) }
    ],
    serviceData: [{ service: '0000180d-0000-1000-8000-00805f9b34fb', data: new Uint8Array([1, 2, 3]) }],
    ...overrides
  }
}

async function settleDeterministic(fixture, promise) {
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 20 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

describe('canonical public ScanQuery v1', () => {
  test('normalizes omitted match-all, rejects ambiguous empty/unknown shapes, and freezes bytes', () => {
    const query = normalizeScanQuery()
    expect(query.anyOf).toBeNull()
    expect(query.exclude).toBeNull()
    expect(Object.isFrozen(query)).toBe(true)
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation()))).toBe(true)

    expect(() => normalizeScanQuery({ anyOf: [] })).toThrow()
    expect(() => normalizeScanQuery({ exclude: [] })).toThrow()
    expect(() => normalizeScanQuery({ anyOf: [{ peers: ['peer'] }] })).toThrow()

    const prefix = new Uint8Array([2])
    const normalized = normalizeScanQuery({
      anyOf: [{ manufacturerData: { any: [{ companyId: 76, dataPrefix: prefix }] } }]
    })
    prefix[0] = 9
    expect([...normalized.anyOf[0].manufacturerData.any[0].dataPrefix]).toEqual([2])
  })

  test('applies clause AND, anyOf OR, field any/all, and exclusion precedence', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          services: { all: ['180d', '180f'] },
          names: { prefixes: ['Heart'] },
          manufacturerData: { all: [{ companyId: 76, dataPrefix: new Uint8Array([2]) }, { companyId: 123 }] },
          serviceData: { any: [{ service: '180d', dataPrefix: new Uint8Array([1, 2]) }] },
          rssi: { minimum: -50, maximum: -30 },
          connectable: true
        },
        { names: { exact: ['Other'] } }
      ],
      exclude: [{ services: { any: ['180f'] } }]
    })
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation()))).toBe(false)
    expect(
      observationMatchesScanQuery(
        normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Heart'] } }], exclude: [{ connectable: false }] }),
        normalizeScanObservation(observation())
      )
    ).toBe(true)
  })

  test('uses masked byte matching and distinguishes absent fields from empty observed data', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          manufacturerData: {
            any: [{ companyId: 76, dataPrefix: new Uint8Array([0x02, 0x10]), mask: new Uint8Array([0xff, 0xf0]) }]
          },
          serviceData: {
            all: [{ service: '180d', mask: new Uint8Array([0xff, 0x0f]), dataPrefix: new Uint8Array([1, 2]) }]
          }
        }
      ]
    })
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation()))).toBe(true)
    expect(
      observationMatchesScanQuery(
        query,
        normalizeScanObservation(observation({ manufacturerData: null, serviceData: null }))
      )
    ).toBe(false)
    expect(() =>
      normalizeScanQuery({ anyOf: [{ manufacturerData: { any: [{ companyId: 1, mask: new Uint8Array([1]) }] } }] })
    ).toThrow()
    expect(() => normalizeScanQuery({ anyOf: [{ rssi: { minimum: 0, maximum: -1 } }] })).toThrow()
  })

  test('produces an order-independent semantic digest and canonical UUIDs', () => {
    const first = normalizeScanQuery({
      anyOf: [{ services: { any: ['180F', '180D', '180D'] }, names: { exact: ['A', 'B'] } }]
    })
    const second = normalizeScanQuery({
      anyOf: [{ names: { exact: ['A', 'B'] }, services: { any: ['180d', '180f'] } }]
    })
    expect(first.digest).toBe(second.digest)
    expect(first.anyOf[0].services.any).toEqual([
      '0000180d-0000-1000-8000-00805f9b34fb',
      '0000180f-0000-1000-8000-00805f9b34fb'
    ])
    expect(first.digest).toMatch(/^scan-query-v1:[0-9a-f]{16}$/)
    const clauses = [{ names: { prefixes: ['Target'] } }, { rssi: { minimum: -60 } }]
    expect(normalizeScanQuery({ anyOf: clauses }).digest).toBe(
      normalizeScanQuery({ anyOf: [...clauses].reverse() }).digest
    )
  })

  test('canonical ordering does not depend on locale collation', () => {
    const originalLocaleCompare = String.prototype.localeCompare
    String.prototype.localeCompare = () => {
      throw new Error('locale-sensitive ordering is forbidden for semantic digests')
    }
    try {
      expect(() => normalizeScanQuery({ anyOf: [{ names: { exact: ['ä', 'z'] } }] })).not.toThrow()
    } finally {
      String.prototype.localeCompare = originalLocaleCompare
    }
  })

  test('rejects malformed normalized advertisement entries before canonical matching', () => {
    expect(() =>
      normalizeScanObservation({
        ...deterministicScenarioAdvertisement(),
        localName: { state: 'invalid' }
      })
    ).toThrow()

    expect(() =>
      normalizeScanObservation({
        ...deterministicScenarioAdvertisement(),
        localName: { state: 'present' }
      })
    ).toThrow()

    expect(() =>
      normalizeScanObservation({
        localName: 'Malformed',
        rssi: null,
        connectable: null,
        serviceUuids: [],
        manufacturerData: [{ companyId: '76', data: new Uint8Array([1]) }],
        serviceData: []
      })
    ).toThrow()

    expect(() =>
      normalizeScanObservation({
        localName: 'Malformed',
        rssi: null,
        connectable: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: [{ service: '180d', data: new Uint8Array([1]) }]
      })
    ).toThrow()

    expect(() => normalizeScanObservation({ device: {} })).toThrow()

    expect(() =>
      normalizeScanObservation({
        localName: 'Scoped',
        rssi: -40,
        connectable: true,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: [],
        peerId: 'must-not-cross-the-boundary'
      })
    ).toThrow()
  })

  test('uses the same normalized query for the public residual stream and find helper', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const filtered = filterScanObservations(
      source,
      normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Target'] } }] })
    )
    const iterator = filtered[Symbol.asyncIterator]()
    const pending = iterator.next()
    source.emit(
      {
        peerId: 'ignored',
        localName: 'Other',
        rssi: -80,
        txPowerLevel: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )
    source.emit(
      {
        peerId: 'target',
        localName: 'Target Device',
        rssi: -40,
        txPowerLevel: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )
    await expect(pending).resolves.toMatchObject({ value: { kind: 'value', value: { peer: { id: 'target' } } } })
    await filtered.close()

    const scan = {
      observations: filtered,
      stop: async () => ({ state: 'released', failures: [] })
    }
    await expect(findPeerInScan(scan, 'first')).rejects.toMatchObject({ code: 'stream.closed' })
  })

  test('coalesces duplicate Tauri-style observations only when requested', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const filtered = filterScanObservations(source, normalizeScanQuery(), 'coalesced')
    const first = filtered[Symbol.asyncIterator]()
    const firstValue = first.next()
    const observation = {
      peerId: 'duplicate-peer',
      localName: 'Duplicate',
      rssi: -40,
      txPowerLevel: null,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }
    source.emit(observation, 32)
    source.emit(observation, 32)
    await expect(firstValue).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: { id: 'duplicate-peer' } } }
    })
    const second = first.next()
    source.closeWithReason('closed')
    await expect(second).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
    await expect(first.next()).resolves.toMatchObject({ done: true })
    await first.return()
  })

  test.each([
    ['by default', {}],
    ['when requested', { duplicates: 'coalesced' }]
  ])('manager coalesces duplicate observations %s', async (_label, scanOptions) => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const scan = await manager.scan(scanOptions)
    const iterator = scan.observations[Symbol.asyncIterator]()
    const firstValue = iterator.next()
    const observation = {
      peerId: 'manager-duplicate-peer',
      localName: 'Manager duplicate',
      rssi: -40,
      txPowerLevel: null,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }

    source.emit(observation, 32)
    await expect(firstValue).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: { id: 'manager-duplicate-peer' } } }
    })

    const secondValue = iterator.next()
    source.emit(observation, 32)
    source.closeWithReason('closed')
    await expect(secondValue).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
    await iterator.return()
    await scan.stop()
  })

  test('coalesced scans deliver changed current-view observations', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const filtered = filterScanObservations(source, normalizeScanQuery(), 'coalesced')
    const iterator = filtered[Symbol.asyncIterator]()
    const firstValue = iterator.next()
    const firstObservation = {
      peerId: 'current-view-peer',
      localName: 'Current view',
      rssi: -40,
      txPowerLevel: null,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }
    source.emit(firstObservation, 32)
    await expect(firstValue).resolves.toMatchObject({ value: { kind: 'value', value: { rssi: -40 } } })

    const changedValue = iterator.next()
    source.emit({ ...firstObservation, rssi: -39 }, 32)
    await expect(changedValue).resolves.toMatchObject({ value: { kind: 'value', value: { rssi: -39 } } })

    const unchangedValue = iterator.next()
    source.emit({ ...firstObservation, rssi: -39 }, 32)
    source.closeWithReason('closed')
    await expect(unchangedValue).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
    await iterator.return()
  })

  test('derives observed and monotonic lost events without stealing observations', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    let now = 0
    let scheduled
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      scheduleDeadline: jest.fn((deadline, action) => {
        scheduled = { deadline, action, cancel: jest.fn() }
        return scheduled
      }),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => now)
    const scan = await manager.scan({ observation: { reportLostAfterMs: 10 } })
    const observations = scan.observations[Symbol.asyncIterator]()
    const events = scan.events[Symbol.asyncIterator]()
    const observationNext = observations.next()
    const discoveredNext = events.next()
    source.emit(
      {
        peerId: 'derived-peer',
        localName: 'Derived peer',
        rssi: -42,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )

    await expect(observationNext).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: { id: 'derived-peer' } } }
    })
    await expect(discoveredNext).resolves.toMatchObject({
      value: { kind: 'observed', peer: { id: 'derived-peer' } }
    })
    expect(scheduled.deadline).toBe(10)

    now = 10
    scheduled.action()
    await expect(events.next()).resolves.toMatchObject({
      value: {
        kind: 'lost',
        peer: { id: 'derived-peer' },
        lastObservedAt: 0,
        derivedAt: 10,
        reason: 'observation-timeout'
      }
    })
    expect(scheduled.cancel).not.toHaveBeenCalled()
    await observations.return()
    await events.return()
    await scan.stop()
  })

  test.each([0, -1, 1.5, Number.MAX_SAFE_INTEGER])('rejects invalid reportLostAfterMs %s', async reportLostAfterMs => {
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)

    await expect(manager.scan({ observation: { reportLostAfterMs } })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(internal.scan).not.toHaveBeenCalled()
  })

  test('emits an identical observation again after a lost transition', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    let now = 0
    const scheduled = []
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      scheduleDeadline: jest.fn((deadline, action) => {
        const handle = { deadline, action, cancel: jest.fn() }
        scheduled.push(handle)
        return handle
      }),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => now)
    const scan = await manager.scan({ observation: { reportLostAfterMs: 10 } })
    const events = scan.events[Symbol.asyncIterator]()
    const observation = {
      peerId: 'reappearing-peer',
      localName: 'Same packet',
      rssi: -42,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }
    const first = events.next()
    source.emit(observation, 32)
    await expect(first).resolves.toMatchObject({ value: { kind: 'observed' } })
    now = 10
    scheduled[0].action()
    await expect(events.next()).resolves.toMatchObject({ value: { kind: 'lost' } })
    const second = events.next()
    source.emit(observation, 32)
    await expect(second).resolves.toMatchObject({
      value: { kind: 'observed', peer: { id: 'reappearing-peer' } }
    })
    expect(scheduled).toHaveLength(2)
    await events.return()
    await scan.stop()
  })

  test('rejects lost reporting with raw duplicate delivery', async () => {
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)

    await expect(manager.scan({ duplicates: 'all', observation: { reportLostAfterMs: 10 } })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(internal.scan).not.toHaveBeenCalled()
  })

  test('validates typed platform controls and rejects unsupported controls before radio work', async () => {
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)

    await expect(
      manager.scan({ platform: { kind: 'android', mode: 'low-latency', callbackType: 'all-matches' } })
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
    await expect(manager.scan({ platform: { kind: 'android', mode: 'invalid' } })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(internal.scan).not.toHaveBeenCalled()
  })

  test('does not silently ignore lost reporting when the public timer authority is absent', async () => {
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)

    await expect(manager.scan({ observation: { reportLostAfterMs: 10 } })).rejects.toMatchObject({
      code: 'capability.unavailable'
    })
    expect(internal.scan).not.toHaveBeenCalled()
  })

  test('propagates source overflow to observations and terminates event subscribers with overflow', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(1), byteCapacity: capacity(128), reservedControlCapacity: capacity(1) },
      'drop-newest'
    )
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const scan = await manager.scan()
    const observation = {
      peerId: 'overflow-peer',
      localName: 'Overflow peer',
      rssi: -42,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }
    source.emit(observation, 32)
    source.emit({ ...observation, peerId: 'overflow-peer-2' }, 32)

    const observations = scan.observations[Symbol.asyncIterator]()
    const events = scan.events[Symbol.asyncIterator]()
    await expect(observations.next()).resolves.toMatchObject({ value: { kind: 'overflow' } })
    await expect(events.next()).rejects.toMatchObject({ code: 'stream.overflow' })
    await scan.stop()
  })

  test('drains an accepted observation before delivering the source terminal', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const scan = await manager.scan()
    const observations = scan.observations[Symbol.asyncIterator]()
    const events = scan.events[Symbol.asyncIterator]()
    const observation = {
      peerId: 'terminal-peer',
      localName: 'Terminal peer',
      rssi: -40,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }
    const observationNext = observations.next()
    const eventNext = events.next()
    source.emit(observation, 32)
    await expect(observationNext).resolves.toMatchObject({ value: { kind: 'value' } })
    await expect(eventNext).resolves.toMatchObject({ value: { kind: 'observed' } })
    source.closeWithReason('closed')
    await expect(observations.next()).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
    await expect(events.next()).resolves.toMatchObject({ done: true })
    await scan.stop()
  })

  test('bounds retained lost-peer state while retaining explicit timer cancellation', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(512), byteCapacity: capacity(1024 * 1024), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const scheduled = []
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      scheduleDeadline: jest.fn((deadline, action) => {
        const handle = { deadline, action, cancel: jest.fn() }
        scheduled.push(handle)
        return handle
      }),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const scan = await manager.scan({ observation: { reportLostAfterMs: 10 } })
    const events = scan.events[Symbol.asyncIterator]()
    const first = events.next()
    for (let index = 0; index < 300; index += 1) {
      source.emit(
        {
          peerId: `bounded-peer-${index}`,
          localName: `Peer ${index}`,
          rssi: -40,
          serviceUuids: [],
          manufacturerData: [],
          serviceData: []
        },
        32
      )
    }
    await first
    for (let ordinal = 0; ordinal < 512 && scheduled.length < 300; ordinal += 1) await Promise.resolve()
    expect(scheduled.length).toBe(300)
    expect(scheduled.filter(handle => handle.cancel.mock.calls.length > 0).length).toBeGreaterThanOrEqual(44)
    await events.return()
    await scan.stop()
  })

  test('preserves source terminal delivery and cleans the lost timer on stop and destroy', async () => {
    const createInternal = () => {
      const source = new CoreBoundedStream(
        { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
        'drop-oldest'
      )
      const scheduled = []
      const internal = {
        identity: null,
        attachedBackend: undefined,
        supports: () => true,
        capability: () => null,
        capabilities: () => [],
        scan: jest.fn(async () => ({
          observations: source,
          stop: async () => ({ state: 'released', failures: [] })
        })),
        scheduleDeadline: jest.fn((deadline, action) => {
          const handle = { deadline, action, cancel: jest.fn() }
          scheduled.push(handle)
          return handle
        }),
        connect: jest.fn(),
        destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
      }
      return { internal, scheduled, source }
    }

    const stopped = createInternal()
    const stoppedManager = await createPublicBleManager(stopped.internal, () => 0)
    const stoppedScan = await stoppedManager.scan({ observation: { reportLostAfterMs: 10 } })
    const stoppedObservations = stoppedScan.observations[Symbol.asyncIterator]()
    const stoppedEvents = stoppedScan.events[Symbol.asyncIterator]()
    const stoppedObservation = stoppedObservations.next()
    const stoppedEvent = stoppedEvents.next()
    stopped.source.emit(
      {
        peerId: 'stop-peer',
        localName: 'Stop peer',
        rssi: -40,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )
    await stoppedObservation
    await stoppedEvent
    await stoppedScan.stop()
    expect(stopped.scheduled[0].cancel).toHaveBeenCalledTimes(1)
    await expect(stoppedEvents.next()).resolves.toMatchObject({ done: true })

    const destroyed = createInternal()
    const destroyedManager = await createPublicBleManager(destroyed.internal, () => 0)
    const destroyedScan = await destroyedManager.scan({ observation: { reportLostAfterMs: 10 } })
    const destroyedEvents = destroyedScan.events[Symbol.asyncIterator]()
    const destroyedNext = destroyedEvents.next()
    destroyed.source.emit(
      {
        peerId: 'destroy-peer',
        localName: 'Destroy peer',
        rssi: -41,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )
    await destroyedNext
    await destroyedManager.destroy()
    expect(destroyed.scheduled[0].cancel).toHaveBeenCalledTimes(1)
    await expect(destroyedEvents.next()).resolves.toMatchObject({ done: true })

    const terminal = createInternal()
    const terminalManager = await createPublicBleManager(terminal.internal, () => 0)
    const terminalScan = await terminalManager.scan({ observation: { reportLostAfterMs: 10 } })
    const terminalObservations = terminalScan.observations[Symbol.asyncIterator]()
    const terminalEvents = terminalScan.events[Symbol.asyncIterator]()
    terminal.source.closeWithReason('operation-timed-out')
    await expect(terminalObservations.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'operation-timed-out' }
    })
    await expect(terminalEvents.next()).resolves.toMatchObject({ done: true })
  })

  test('public deterministic coalesced scans deliver RSSI-only changes and suppress unchanged repeats', async () => {
    const { manager, fixture } = await createDeterministicTestBleManager()
    let scan
    try {
      scan = await settleDeterministic(fixture, manager.scan({ duplicates: 'coalesced' }))
      const iterator = scan.observations[Symbol.asyncIterator]()
      const firstObservation = {
        ...deterministicScenarioAdvertisement(),
        rssi: { state: 'present', value: -40, provenance: 'observed' }
      }
      const firstValue = iterator.next()
      fixture.controller.emitAdvertisement(firstObservation)
      await expect(firstValue).resolves.toMatchObject({ value: { kind: 'value', value: { rssi: -40 } } })

      const changedValue = iterator.next()
      fixture.controller.emitAdvertisement({
        ...firstObservation,
        rssi: { state: 'present', value: -39, provenance: 'observed' }
      })
      await expect(
        Promise.race([
          changedValue,
          new Promise(resolve => setTimeout(() => resolve({ done: false, value: { kind: 'timeout' } }), 100))
        ])
      ).resolves.toMatchObject({ value: { kind: 'value', value: { rssi: -39 } } })

      const unchangedValue = iterator.next()
      fixture.controller.emitAdvertisement({
        ...firstObservation,
        rssi: { state: 'present', value: -39, provenance: 'observed' }
      })
      await settleDeterministic(fixture, scan.stop())
      await expect(unchangedValue).resolves.toMatchObject({ value: { kind: 'terminal' } })
      await iterator.return()
    } finally {
      if (scan !== undefined) await settleDeterministic(fixture, scan.stop())
      await settleDeterministic(fixture, manager.destroy())
    }
  })

  function createCoalescedLostScan() {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(65536), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    let now = 0
    const scheduled = []
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      capability: () => null,
      capabilities: () => [],
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({ state: 'released', failures: [] })
      })),
      scheduleDeadline: jest.fn((deadline, action) => {
        const handle = { deadline, action, cancel: jest.fn() }
        scheduled.push(handle)
        return handle
      }),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    return {
      source,
      scheduled,
      internal,
      now: () => now,
      advance(ms) {
        now += ms
      }
    }
  }

  function advertisement(peerId, overrides = {}) {
    return {
      peerId,
      localName: overrides.localName ?? null,
      rssi: overrides.rssi ?? -40,
      serviceUuids: [],
      manufacturerData: overrides.manufacturerData ?? [],
      serviceData: []
    }
  }

  async function expectAccounting(scan) {
    const accounting = inspectPublicScanFingerprintAccountingForTests(scan)
    expect(accounting.fingerprintBytes).toBe(accounting.summedEntryBytes)
    expect(accounting.fingerprintBytes).toBeGreaterThanOrEqual(0)
    expect(accounting.fingerprintCount).toBeGreaterThanOrEqual(0)
    return accounting
  }

  test('lost-peer fingerprint churn keeps coalescing a later stable peer', async () => {
    const fixture = createCoalescedLostScan()
    const manager = await createPublicBleManager(fixture.internal, fixture.now)
    const scan = await manager.scan({ duplicates: 'coalesced', observation: { reportLostAfterMs: 10 } })
    const observations = scan.observations[Symbol.asyncIterator]()
    const manufacturerData = [{ companyId: 1, data: new Uint8Array(256).fill(9) }]

    for (let index = 0; index < 400; index += 1) {
      const next = observations.next()
      fixture.source.emit(advertisement(`lost-${index}`, { manufacturerData }), 320)
      await expect(next).resolves.toMatchObject({
        value: { kind: 'value', value: { peer: { id: `lost-${index}` } } }
      })
      await expectAccounting(scan)
      fixture.advance(10)
      fixture.scheduled[fixture.scheduled.length - 1].action()
      await expectAccounting(scan)
    }

    const first = observations.next()
    fixture.source.emit(advertisement('stable', { localName: 'Stable', manufacturerData }), 320)
    await expect(first).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: { id: 'stable' } } }
    })
    expect((await expectAccounting(scan)).fingerprintCount).toBe(1)
    const second = observations.next()
    fixture.source.emit(advertisement('stable', { localName: 'Stable', manufacturerData }), 320)
    for (let attempt = 0; attempt < 10; attempt += 1) await Promise.resolve()
    await scan.stop()
    await expect(second).resolves.toMatchObject({ value: { kind: 'terminal' } })
    expect(inspectPublicScanFingerprintAccountingForTests(scan)).toEqual({
      fingerprintCount: 0,
      fingerprintBytes: 0,
      summedEntryBytes: 0
    })
    await observations.return()
    await manager.destroy()
  })

  test('presence-cap eviction churn keeps coalescing a later stable peer', async () => {
    const fixture = createCoalescedLostScan()
    const manager = await createPublicBleManager(fixture.internal, fixture.now)
    const scan = await manager.scan({ duplicates: 'coalesced', observation: { reportLostAfterMs: 10 } })
    const observations = scan.observations[Symbol.asyncIterator]()

    for (let index = 0; index < 256 + 1100; index += 1) {
      const next = observations.next()
      fixture.source.emit(advertisement(`present-${index}`), 32)
      await expect(next).resolves.toMatchObject({
        value: { kind: 'value', value: { peer: { id: `present-${index}` } } }
      })
      await expectAccounting(scan)
    }

    const first = observations.next()
    fixture.source.emit(advertisement('stable'), 32)
    await expect(first).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: { id: 'stable' } } }
    })
    expect((await expectAccounting(scan)).fingerprintCount).toBeGreaterThan(0)
    const second = observations.next()
    fixture.source.emit(advertisement('stable'), 32)
    for (let attempt = 0; attempt < 10; attempt += 1) await Promise.resolve()
    await scan.stop()
    await expect(second).resolves.toMatchObject({ value: { kind: 'terminal' } })
    expect(inspectPublicScanFingerprintAccountingForTests(scan)).toEqual({
      fingerprintCount: 0,
      fingerprintBytes: 0,
      summedEntryBytes: 0
    })
    await observations.return()
    await manager.destroy()
    expect(fixture.scheduled.some(handle => handle.cancel.mock.calls.length > 0)).toBe(true)
  })

  test('fingerprint replacement and source terminal keep exact byte accounting', async () => {
    const fixture = createCoalescedLostScan()
    const manager = await createPublicBleManager(fixture.internal, fixture.now)
    const scan = await manager.scan({ duplicates: 'coalesced', observation: { reportLostAfterMs: 10 } })
    const observations = scan.observations[Symbol.asyncIterator]()
    const first = observations.next()
    fixture.source.emit(advertisement('peer-1', { rssi: -40 }), 32)
    await first
    const afterFirst = await expectAccounting(scan)
    expect(afterFirst.fingerprintCount).toBe(1)
    expect(afterFirst.fingerprintBytes).toBeGreaterThan(0)
    const second = observations.next()
    fixture.source.emit(advertisement('peer-1', { rssi: -39 }), 32)
    await second
    const afterReplace = await expectAccounting(scan)
    expect(afterReplace.fingerprintCount).toBe(1)
    fixture.source.closeWithReason('closed')
    await expect(observations.next()).resolves.toMatchObject({ value: { kind: 'terminal' } })
    expect(inspectPublicScanFingerprintAccountingForTests(scan)).toEqual({
      fingerprintCount: 0,
      fingerprintBytes: 0,
      summedEntryBytes: 0
    })
    await manager.destroy()
  })
})
