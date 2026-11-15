const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')

const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'
const options = signal => ({ signal: signal ?? null, deadline: null, delivery: { itemCapacity: 1, byteCapacity: 16, reservedControlCapacity: 1, overflowPolicy: 'error' }, deliveryMode: 'prefer-notification' })

function deferred() {
  let resolve
  const promise = new Promise(r => { resolve = r })
  return { promise, resolve }
}

async function fixture() {
  const listeners = new Set()
  const characteristic = {
    uuid: CHARACTERISTIC,
    properties: { read: false, write: false, writeWithoutResponse: false, notify: true, indicate: false },
    getDescriptors: async () => [],
    startNotifications: jest.fn(async () => {}),
    stopNotifications: jest.fn(async () => {}),
    addNotificationListener: listener => listeners.add(listener),
    removeNotificationListener: listener => listeners.delete(listener)
  }
  const service = { uuid: SERVICE, getCharacteristics: async () => [characteristic] }
  const gatt = { connected: false, connect: async () => { gatt.connected = true }, disconnect: () => { gatt.connected = false }, getPrimaryServices: async () => [service] }
  const device = { id: 'device', gatt, addDisconnectListener: () => {}, removeDisconnectListener: () => {} }
  const boundary = { implementationVersion: 'test', browserEngine: 'test', isSecureContext: () => true, hasTransientUserActivation: () => true, bluetoothAvailable: async () => true, requestDevice: async () => ({ device, grantedServices: [SERVICE] }), now: () => 10, setTimer: () => ({}), clearTimer: () => {}, addPageLifecycleListener: () => () => {} }
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const selected = await backend.choose({ filters: [{ serviceUuids: [SERVICE], manufacturerData: [], localNamePrefix: null }], acceptAllDevices: false, optionalServices: [SERVICE] }, { signal: null, deadline: null })
  const connection = (await backend.connections.connect(selected.peerId, 'client', { signal: null, deadline: null })).connection
  const database = await backend.gatt.discover(connection, { signal: null, deadline: null })
  const path = (await database.snapshot()).characteristics[0].path
  return { backend, connection, database, path, characteristic, emit: bytes => { for (const listener of listeners) listener(new Uint8Array(bytes)) } }
}

function reserve(f, lifecycle, signal) {
  return f.backend.gattRuntime.reserveRecordingMember(f.connection, f.database, { memberId: 'heart', path: f.path, deliveryMode: 'prefer-notification' }, options(signal), lifecycle)
}

test('installs retained ingress before enabling and shares the existing physical owner', async () => {
  const f = await fixture()
  try {
    const ordinary = await f.database.subscribe(f.path, { ...options(), delivery: { ...options().delivery, itemCapacity: 8 } })
    let installed = false
    const member = reserve(f, { isInstalled: () => installed })
    await expect(member.lease.enable()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    const ingress = f.backend.gattRuntime.notificationIngress(f.database).registerRetained(new Map([[member.characteristic, member.id]]), 4, 16)
    installed = true
    await member.lease.enable()
    for (let i = 0; i < 3; i++) f.emit([i])
    expect(ingress.peek().record).toMatchObject({ kind: 'value', memberId: 'heart', value: new Uint8Array([0]) })
    expect(f.backend.gattRuntime.subscriptionCount()).toBe(2)
    expect(f.characteristic.startNotifications).toHaveBeenCalledTimes(1)
    await member.lease.release()
    expect(f.characteristic.stopNotifications).not.toHaveBeenCalled()
    expect(await ordinary.remove()).toMatchObject({ state: 'released' })
    expect(f.characteristic.stopNotifications).toHaveBeenCalledTimes(1)
  } finally { await f.backend.destroy() }
})

test('release before enable fences the reservation and repeated enable cannot acquire twice', async () => {
  const f = await fixture()
  try {
    const unused = reserve(f, { isInstalled: () => true })
    await unused.lease.release()
    await expect(unused.lease.enable()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    const member = reserve(f, { isInstalled: () => true })
    await member.lease.enable()
    await expect(member.lease.enable()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    expect(f.characteristic.startNotifications).toHaveBeenCalledTimes(1)
    await member.lease.release()
    await member.lease.release()
    expect(f.characteristic.stopNotifications).toHaveBeenCalledTimes(1)
  } finally { await f.backend.destroy() }
})

test('rejects foreign or stale connection and graph before reservation or enable', async () => {
  const f = await fixture()
  try {
    expect(() => f.backend.gattRuntime.reserveRecordingMember({ ...f.connection }, f.database, { memberId: 'heart', path: f.path, deliveryMode: 'prefer-notification' }, options(), { isInstalled: () => true })).toThrow()
    expect(() => reserve({ ...f, path: { ...f.path, databaseGeneration: 'wrong' } }, { isInstalled: () => true })).toThrow()
    const member = reserve(f, { isInstalled: () => true })
    await f.backend.gatt.discover(f.connection, { signal: null, deadline: null })
    await expect(member.lease.enable()).rejects.toThrow()
    expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
    await member.lease.release()
  } finally { await f.backend.destroy() }
})

test('cancelled pending start remains owned, reports pending cleanup and reconciles a late start', async () => {
  const f = await fixture()
  const start = deferred()
  f.characteristic.startNotifications.mockImplementation(() => start.promise)
  const controller = new AbortController()
  try {
    const member = reserve(f, { isInstalled: () => true }, controller.signal)
    const enabling = member.lease.enable()
    await Promise.resolve()
    controller.abort()
    await expect(enabling).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(member.lease.release()).rejects.toMatchObject({ cleanup: { state: 'release-failed' } })
    start.resolve()
    for (let i = 0; i < 8; i++) await Promise.resolve()
    await expect(member.lease.release()).resolves.toBeUndefined()
    expect(f.characteristic.stopNotifications).toHaveBeenCalledTimes(1)
    expect(f.backend.gattRuntime.subscriptionCount()).toBe(0)
  } finally { start.resolve(); await f.backend.destroy() }
})

test('reports a physical stop failure and retries the same owned lease', async () => {
  const f = await fixture()
  try {
    f.characteristic.stopNotifications.mockRejectedValueOnce(new Error('stop failed'))
    const member = reserve(f, { isInstalled: () => true })
    await member.lease.enable()
    await expect(member.lease.release()).rejects.toMatchObject({ cleanup: { state: 'release-failed', failures: expect.any(Array) } })
    expectConsoleErrorMatching('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    await expect(member.lease.release()).resolves.toBeUndefined()
    expect(f.characteristic.stopNotifications).toHaveBeenCalledTimes(2)
  } finally { await f.backend.destroy() }
})

test('release while waiting for another physical stop prevents a late enable', async () => {
  const f = await fixture()
  const stop = deferred()
  try {
    const ordinary = await f.database.subscribe(f.path, options())
    f.characteristic.stopNotifications.mockImplementationOnce(() => stop.promise)
    const removing = ordinary.remove()
    await Promise.resolve()
    const member = reserve(f, { isInstalled: () => true })
    const enabling = member.lease.enable()
    await expect(member.lease.release()).rejects.toMatchObject({ cleanup: { state: 'release-failed' } })
    stop.resolve()
    await removing
    await expect(enabling).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    await expect(member.lease.release()).resolves.toBeUndefined()
    expect(f.characteristic.startNotifications).toHaveBeenCalledTimes(1)
  } finally { stop.resolve(); await f.backend.destroy() }
})

test('captures synchronous startup notifications in retained ingress without an ordinary consumer', async () => {
  const f = await fixture()
  try {
    const member = reserve(f, { isInstalled: () => true })
    const ingress = f.backend.gattRuntime.notificationIngress(f.database).registerRetained(new Map([[member.characteristic, member.id]]), 4, 16)
    f.characteristic.startNotifications.mockImplementation(async () => { f.emit([7]); f.emit([8]) })
    await member.lease.enable()
    const first = ingress.peek()
    expect(first.record).toMatchObject({ kind: 'value', value: new Uint8Array([7]) })
    expect(ingress.ackExact(first.id)).toBe(true)
    expect(ingress.peek().record).toMatchObject({ kind: 'value', value: new Uint8Array([8]) })
    expect(f.backend.gattRuntime.retainedSubscriptionCount()).toBe(0)
    await member.lease.release()
  } finally { await f.backend.destroy() }
})

test('rechecks live authorization at the physical start dispatch microtask', async () => {
  const f = await fixture()
  let live = true
  try {
    const member = reserve(f, { isInstalled: () => live })
    const enabling = member.lease.enable()
    live = false
    await expect(enabling).rejects.toThrow()
    expectConsoleErrorMatching('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    await member.lease.release()
    expect(f.characteristic.startNotifications).not.toHaveBeenCalled()
  } finally { await f.backend.destroy() }
})

test('a fenced recording does not block another live consumer sharing the pending physical start', async () => {
  const f = await fixture()
  let firstLive = true
  try {
    const first = reserve(f, { isInstalled: () => firstLive })
    const second = reserve(f, { isInstalled: () => true })
    const firstEnable = first.lease.enable()
    const secondEnable = second.lease.enable()
    firstLive = false
    await expect(firstEnable).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    await secondEnable
    expect(f.characteristic.startNotifications).toHaveBeenCalledTimes(1)
    await first.lease.release()
    expect(f.characteristic.stopNotifications).not.toHaveBeenCalled()
    await second.lease.release()
    expect(f.characteristic.stopNotifications).toHaveBeenCalledTimes(1)
  } finally { await f.backend.destroy() }
})
