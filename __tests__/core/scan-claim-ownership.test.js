const { rustCoreHarness, environment, settle, scanOptions } = require('../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../src/react-native-manager')
const { createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } = require('../../src/manager/ble-manager')
const { opaqueId } = require('../../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')

test('core keeps a claimable backend observation charged while copying it into the core stream', async () => {
  const h = rustCoreHarness()
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const attached = internal.attachedBackend
  const core = await createBleManager({ attachedBackend: attached, clientId: opaqueId('claim-client', 'client', 'test'), managerId: opaqueId('claim-manager', 'manager', 'test'), ownerMode: 'owning' }, createManagerOwnershipAuthority(attached), { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => 1000 })
  const backend = attached.backend
  const scanner = backend.scanner
  let source
  let nativeIterator
  backend.scanner = { ...scanner, start: async (...args) => {
    const lease = await scanner.start(...args)
    nativeIterator = lease.observations[Symbol.asyncIterator]()
    source = new CoreBoundedStream(lease.observations.limits, 'error', true)
    return { ...lease, observations: source }
  } }
  const scan = await core.scan(scanOptions())
  try {
    h.native.emitAdvertisement()
    const original = (await nativeIterator.next()).value.value
    let chargeWhileCopying = 0
    const observation = { ...original, get rssi() { chargeWhileCopying = source.retainedBytes(); return original.rssi } }
    source.emit(observation, 100)
    await settle(100)
    expect(chargeWhileCopying).toBeGreaterThanOrEqual(100)
    expect((await scan.observations[Symbol.asyncIterator]().next()).value.kind).toBe('value')
  } finally {
    await source.close()
    await nativeIterator.return()
    await scan.stop()
    await core.destroy()
    await internal.destroy()
  }
})

test('core rolls a backend claim back when its aggregate quota refuses the observation', async () => {
  const h = rustCoreHarness()
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const attached = internal.attachedBackend
  const core = await createBleManager({ attachedBackend: attached, clientId: opaqueId('refusal-client', 'client', 'test'), managerId: opaqueId('refusal-manager', 'manager', 'test'), ownerMode: 'owning' }, createManagerOwnershipAuthority(attached), { ...DEFAULT_BLE_MANAGER_OPTIONS, maximumAggregateRetainedBytes: 10, now: () => 1000 })
  const backend = attached.backend
  const scanner = backend.scanner
  let source
  let nativeIterator
  backend.scanner = { ...scanner, start: async (...args) => {
    const lease = await scanner.start(...args)
    nativeIterator = lease.observations[Symbol.asyncIterator]()
    source = new CoreBoundedStream(lease.observations.limits, 'error', true)
    return { ...lease, observations: source }
  } }
  const scan = await core.scan(scanOptions())
  try {
    h.native.emitAdvertisement()
    const original = (await nativeIterator.next()).value.value
    source.emit(original, 100)
    await settle(100)
    expect((await scan.observations[Symbol.asyncIterator]().next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 1 })
    expect(source.retainedBytes()).toBeGreaterThanOrEqual(100)
    expect((await source[Symbol.asyncIterator]().next()).value.value).toBe(original)
  } finally {
    await source.close()
    await nativeIterator.return()
    await scan.stop()
    await core.destroy()
    await internal.destroy()
  }
})

test('core settles a drop-newest source occurrence once without starving the event loop', async () => {
  const h = rustCoreHarness()
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const attached = internal.attachedBackend
  const core = await createBleManager({ attachedBackend: attached, clientId: opaqueId('drop-client', 'client', 'test'), managerId: opaqueId('drop-manager', 'manager', 'test'), ownerMode: 'owning' }, createManagerOwnershipAuthority(attached), { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => 1000 })
  const backend = attached.backend
  const scanner = backend.scanner
  let source
  let nativeIterator
  backend.scanner = { ...scanner, start: async (...args) => {
    const lease = await scanner.start(...args)
    nativeIterator = lease.observations[Symbol.asyncIterator]()
    source = new CoreBoundedStream(lease.observations.limits, 'error', true)
    return { ...lease, observations: source }
  } }
  const scan = await core.scan(scanOptions({ delivery: { itemCapacity: 1, byteCapacity: 4096, reservedControlCapacity: 1, overflowPolicy: 'drop-newest' } }))
  const quota = core.core.aggregateQuota
  const emit = quota.emit.bind(quota)
  let calls = 0
  const spy = jest.spyOn(quota, 'emit').mockImplementation((...args) => {
    if (++calls > 3) throw new Error('repeated source claim')
    return emit(...args)
  })
  try {
    h.native.emitAdvertisement()
    const original = (await nativeIterator.next()).value.value
    source.emit(original, 100)
    await settle(30)
    source.emit(original, 100)
    await settle(100)
    expect(calls).toBe(2)
    expect(source.retainedPayloadBytes()).toBe(0)
    const iterator = scan.observations[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'overflow', droppedItems: 1 })
    expect((await iterator.next()).value.kind).toBe('value')
    await iterator.return()
  } finally {
    spy.mockRestore()
    await source.close()
    await nativeIterator.return()
    await scan.stop()
    await core.destroy()
    await internal.destroy()
  }
})
