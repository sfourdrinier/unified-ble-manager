// __tests__/backend-contract/AdapterAuthorization.test.js

/**
 * `authorization: 'unknown'` is the absence of a measurement, exactly as it is
 * on the `availability` and `power` siblings. It is never a denial, so no
 * readiness gate may block on it, and a backend with nothing to measure — BlueZ
 * has no per-application Bluetooth authorization concept — must report it
 * rather than substitute `'granted'`.
 */

const { attachBackend } = require('../../src/backend-contract/backend')
const { isAuthorizationBlocking } = require('../../src/backend-contract/identity')
const { capacity, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const {
  attachBleBackend,
  BleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { createBluezBackendProvider } = require('../../src/backends/bluez/bluez-backend-provider')
const { createCoreBluetoothBackendProvider } = require('../../src/backends/corebluetooth/corebluetooth-provider')
const { prepareNativeCoreBluetoothBoundary } = require('../../src/node-corebluetooth')
const { assertWinRtAdapterReady, winRtAdapterIsReady } = require('../../src/backends/winrt/winrt-adapter-state')
const { ReactNativeAppleProtocolBoundary } = require('../../src/native-protocol/rn-apple-boundary')
const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')
const {
  BLUEZ_ADAPTER_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../test-support/bluez/in-memory-bluez-object-manager')
const { InMemoryCoreBluetoothBoundary } = require('../../test-support/corebluetooth/in-memory-corebluetooth-boundary')

const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const noAuthorizationConcept = 'BlueZ exposes no per-application Bluetooth authorization concept'

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function delivery() {
  return {
    itemCapacity: capacity(4),
    byteCapacity: capacity(1024),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
  }
}

function operation() {
  return { signal: null, deadline: null }
}

function scanOptions() {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: delivery(),
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

function adapterSnapshot(authorization) {
  return { availability: 'available', authorization, power: 'on', safeReason: null }
}

async function settle(controller, promise) {
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
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

async function flushMicrotasks() {
  for (let turn = 0; turn < 12; turn += 1) {
    await Promise.resolve()
  }
}

async function managerFixture() {
  const fixture = createDeterministicTestBackend()
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const manager = await BleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('authorization-client', 'client', 'deterministic:authorization'),
      managerId: opaqueId('authorization-manager', 'manager', 'deterministic:authorization'),
      ownerMode: 'owning'
    },
    authority,
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  return { fixture, manager }
}

async function coreBluetoothFixture() {
  let boundary = null
  const provider = createCoreBluetoothBackendProvider({
    boundaryFactory: () => {
      boundary = new InMemoryCoreBluetoothBoundary({ serviceUuid, characteristicUuid })
      return boundary
    },
    now: () => 20,
    hostKind: 'node'
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
  })
  await attachBackend(backend, compatibility())
  return { backend, boundary }
}

function emitAdapterState(boundary, state) {
  boundary.adapter = state
  for (const listener of boundary.adapterStateListeners) {
    listener(state)
  }
}

async function flushAdapterLossCleanup() {
  for (let index = 0; index < 12; index += 1) {
    await Promise.resolve()
  }
}

function bluezAdapter(path, address, alias, powered) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_ADAPTER_INTERFACE,
        properties: {
          Address: { signature: 's', value: address },
          Alias: { signature: 's', value: alias },
          Powered: { signature: 'b', value: powered }
        }
      }
    ]
  }
}

function bluezProviderFor(objects) {
  const boundary = new InMemoryBluezBoundary({ busKind: 'system', objects })
  return createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([
      boundary,
      new InMemoryBluezBoundary({ busKind: 'system', objects })
    ]),
    now: () => 10
  })
}

class StubbedAppleBoundary extends ReactNativeAppleProtocolBoundary {
  constructor(snapshot) {
    super({}, 'authorization-owner')
    this.stubbedSnapshot = snapshot
  }

  adapterSnapshot() {
    return this.stubbedSnapshot
  }
}

describe('adapter authorization vocabulary', () => {
  test.each([
    ['granted', false],
    ['unknown', false],
    ['not-determined', false],
    ['denied', true],
    ['restricted', true],
    ['unavailable', true]
  ])('isAuthorizationBlocking(%s) is %s', (authorization, blocking) => {
    expect(isAuthorizationBlocking(authorization)).toBe(blocking)
  })

  // Only an explicit negative blocks. 'unknown' was never measured and
  // 'not-determined' has not been decided yet; blocking the latter would stop
  // the platform prompt from ever being raised, leaving it undecided forever.
  test('blocks only the values that are an explicit platform refusal', () => {
    const vocabulary = ['granted', 'denied', 'restricted', 'not-determined', 'unavailable', 'unknown']
    expect(vocabulary.filter(value => isAuthorizationBlocking(value))).toEqual([
      'denied',
      'restricted',
      'unavailable'
    ])
  })
})

describe('unknown authorization never makes an adapter unready', () => {
  test('the core keeps every resource when a backend reports an unmeasurable authorization', async () => {
    const { fixture, manager } = await managerFixture()
    const peerId = opaqueId('deterministic-peer', 'peer', 'deterministic')
    const connection = await settle(fixture.controller, manager.connect(peerId, operation()))
    const events = connection.events[Symbol.asyncIterator]()
    await events.next()

    fixture.controller.setAdapterState('available', 'unknown', 'on', noAuthorizationConcept)
    await flushMicrotasks()

    expect(Number(manager.localResourceCounters().connectionLeases)).toBe(1)
    expect(Number(fixture.backend.resourceCounters().physicalLinks)).toBe(1)

    await settle(fixture.controller, manager.destroy())
  })

  test('the core still releases every resource when authorization is explicitly refused', async () => {
    const { fixture, manager } = await managerFixture()
    const peerId = opaqueId('deterministic-peer', 'peer', 'deterministic')
    const connection = await settle(fixture.controller, manager.connect(peerId, operation()))
    const events = connection.events[Symbol.asyncIterator]()
    await events.next()

    fixture.controller.setAdapterState('available', 'denied', 'on', 'deterministic authorization loss')

    await expect(settle(fixture.controller, events.next())).resolves.toMatchObject({
      done: false,
      value: { kind: 'value', value: { cause: 'adapter-loss' } }
    })
    expect(Number(manager.localResourceCounters().connectionLeases)).toBe(0)

    await settle(fixture.controller, manager.destroy())
  })

  test('the node CoreBluetooth boundary accepts an unmeasurable authorization as its first usable state', async () => {
    const state = adapterSnapshot('unknown')
    const remove = jest.fn()
    const boundary = {
      adapterSnapshot: () => state,
      onAdapterState: listener => {
        listener(state)
        return remove
      }
    }

    await expect(prepareNativeCoreBluetoothBoundary(boundary)).resolves.toBeUndefined()
    expect(remove).toHaveBeenCalledTimes(1)
  })

  test('the node CoreBluetooth boundary still waits out an explicit refusal', async () => {
    jest.useFakeTimers()
    try {
      const state = adapterSnapshot('denied')
      const pending = prepareNativeCoreBluetoothBoundary({
        adapterSnapshot: () => state,
        onAdapterState: () => () => {}
      })
      const assertion = expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
      jest.advanceTimersByTime(10_000)
      await assertion
    } finally {
      jest.useRealTimers()
    }
  })

  test('the CoreBluetooth backend does not treat an unmeasurable authorization as adapter loss', async () => {
    const { backend, boundary } = await coreBluetoothFixture()
    const attachmentBefore = backend.attachment().attachmentId

    emitAdapterState(boundary, {
      availability: 'available',
      authorization: 'unknown',
      power: 'on',
      safeReason: 'The test radio reports no authorization concept.'
    })
    await flushAdapterLossCleanup()

    expect(backend.attachment().attachmentId).toBe(attachmentBefore)
    const scan = await backend.scanner.start(scanOptions(), opaqueId('unknown-scan', 'client', 'corebluetooth:auth'))
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })

    await backend.destroy()
  })

  test('the CoreBluetooth backend still treats an explicit refusal as adapter loss', async () => {
    const { backend, boundary } = await coreBluetoothFixture()
    const attachmentBefore = backend.attachment().attachmentId

    emitAdapterState(boundary, {
      availability: 'available',
      authorization: 'denied',
      power: 'on',
      safeReason: 'The test radio was refused Bluetooth access.'
    })
    await flushAdapterLossCleanup()

    expect(backend.attachment().attachmentId).not.toBe(attachmentBefore)
    await expect(
      backend.scanner.start(scanOptions(), opaqueId('undecided-scan', 'client', 'corebluetooth:auth'))
    ).rejects.toMatchObject({ normalized: { code: 'permission.denied' } })

    await backend.destroy()
  })

  test('the WinRT readiness predicate accepts an unmeasurable authorization', () => {
    expect(winRtAdapterIsReady(adapterSnapshot('unknown'))).toBe(true)
    expect(winRtAdapterIsReady(adapterSnapshot('granted'))).toBe(true)
    // Undecided is not a refusal: the prompt is raised by using the radio.
    expect(winRtAdapterIsReady(adapterSnapshot('not-determined'))).toBe(true)
    expect(winRtAdapterIsReady(adapterSnapshot('denied'))).toBe(false)
  })

  test('the WinRT admission cascade admits unknown and preserves every rejection it already made', () => {
    expect(() => assertWinRtAdapterReady(adapterSnapshot('unknown'), 'scan.start')).not.toThrow()

    const rejections = [
      ['denied', 'permission.denied'],
      ['restricted', 'permission.restricted'],
      ['unavailable', 'adapter.unavailable']
    ]
    for (const [authorization, code] of rejections) {
      expect(() => assertWinRtAdapterReady(adapterSnapshot(authorization), 'scan.start')).toThrow(
        expect.objectContaining({ normalized: expect.objectContaining({ code }) })
      )
    }
  })

  test('the React Native Apple boundary admits unknown and preserves every rejection it already made', async () => {
    await expect(new StubbedAppleBoundary(adapterSnapshot('unknown')).startScan(() => {}, [])).rejects.toMatchObject({
      normalized: { code: 'lifecycle.destroyed' }
    })

    const rejections = [
      ['denied', 'permission.denied'],
      ['restricted', 'permission.restricted'],
      ['unavailable', 'permission.not-determined']
    ]
    for (const [authorization, code] of rejections) {
      await expect(
        new StubbedAppleBoundary(adapterSnapshot(authorization)).startScan(() => {}, [])
      ).rejects.toMatchObject({ normalized: { code } })
    }
  })
})

describe('BlueZ reports an unmeasurable authorization', () => {
  test('lists adapters with unknown authorization and discloses why', async () => {
    const provider = bluezProviderFor([bluezAdapter('/org/bluez/hci0', '00:00:00:00:00:01', 'primary', true)])

    const [adapter] = await provider.listAdapters()

    expect(adapter.state.authorization).toBe('unknown')
    expect(adapter.state.availability).toBe('available')
    expect(adapter.state.power).toBe('on')
    expect(adapter.state.safeReason).toBe(noAuthorizationConcept)
  })

  test('keeps the powered-off reason alongside the authorization disclosure', async () => {
    const provider = bluezProviderFor([bluezAdapter('/org/bluez/hci0', '00:00:00:00:00:01', 'primary', false)])

    const [adapter] = await provider.listAdapters()

    expect(adapter.state.authorization).toBe('unknown')
    expect(adapter.state.power).toBe('off')
    expect(adapter.state.safeReason).toBe(`BlueZ adapter is powered off; ${noAuthorizationConcept}`)
  })

  test('reports the same unmeasurable authorization from the live runtime state', async () => {
    const provider = bluezProviderFor([bluezAdapter('/org/bluez/hci0', '00:00:00:00:00:01', 'primary', true)])
    const backend = await provider.create({ selectedAdapterId: opaqueId('/org/bluez/hci0', 'adapter', 'bluez') })

    const state = await backend.adapter.currentState()

    expect(state.authorization).toBe('unknown')
    expect(state.availability).toBe('available')
    expect(state.power).toBe('on')
    expect(state.safeReason).toBe(noAuthorizationConcept)

    await backend.destroy()
  })
})

/**
 * A boundary that has not yet been handed the radio's state reports a PENDING
 * snapshot. Pending is the absence of a measurement, so every field must say
 * so - including authorization. Reporting it as 'unavailable' made the readiness
 * gate gate raise permission.denied for a radio that is switched on with every
 * permission granted, which is the one thing this file says must never happen.
 */
describe('a pending adapter snapshot reports unmeasured authorization', () => {
  test('React Native Android reports every field unmeasured before the radio speaks', () => {
    const boundary = new ReactNativeAndroidProtocolBoundary({}, 'pending-adapter-owner')

    const snapshot = boundary.adapterSnapshot()

    expect(snapshot.availability).toBe('unknown')
    expect(snapshot.power).toBe('unknown')
    expect(snapshot.authorization).toBe('unknown')
    expect(snapshot.safeReason).toBe('The Android radio has not emitted its authoritative adapter state yet.')
  })

  test('the pending authorization does not block readiness', () => {
    const boundary = new ReactNativeAndroidProtocolBoundary({}, 'pending-adapter-owner')

    expect(isAuthorizationBlocking(boundary.adapterSnapshot().authorization)).toBe(false)
  })
})
