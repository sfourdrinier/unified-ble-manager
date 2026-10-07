'use strict'

// Joined public/provider/addon/native-owner tests. Synthetic native transport
// evidence does not substitute for the Linux Node/Bun acquired-FD radio gate.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

async function opened() {
  const harness = h.realBinding('bluez')
  const now = () => performance.now()
  const manager = await createPublicBleManager(
    await createNodeBleManagerFromProvider(
      createTestDesktopRustCoreBackendProvider({
        platform: 'bluez',
        owner: 'acquired-public',
        now,
        radio: 'synthetic',
        binding: harness.binding,
        hostPlatform: 'linux'
      }),
      DESKTOP_RUST_CORE_PROFILES.bluez.compatibility,
      { now }
    ),
    now
  )
  const stage = harness.opened.at(-1)
  const scan = await manager.scan()
  const iterator = scan.observations[Symbol.asyncIterator]()
  const observed = h.nextValue(iterator, 5000)
  await stage.stageAdvertisement({ peerId: 'fd-peer', localName: 'FD peer' })
  const peer = (await observed).peer
  await iterator.return?.()
  await scan.stop()
  const connection = await manager.connect(peer)
  await stage.stageServices('fd-peer', h.hrmServices())
  await stage.stageAcquiredGatt('fd-peer', { write: true, notify: true, mtu: 23 })
  const database = await connection.discover()
  return { manager, connection, stage, peer, database, characteristic: database.characteristic('180d', '2a37') }
}

test('owned acquired write reports actual MTU, copies before waiting and releases after close', async () => {
  const { manager, connection, stage, characteristic } = await opened()
  try {
    const writer = await characteristic.acquireWrite()
    expect(writer.mtuBytes).toBe(23)
    await stage.stageAcquiredBackpressure(true)
    const bytes = new Uint8Array([42])
    const pending = writer.write(bytes, { timeoutMs: 5000 })
    bytes[0] = 99
    await stage.stageAcquiredBackpressure(false)
    await expect(pending).resolves.toMatchObject({ commitState: 'unknown' })
    expect((await stage.stagedAcquiredWriteValues()).map(value => [...value])).toEqual([[42]])
    await expect(writer.write(new Uint8Array(21))).rejects.toMatchObject({ code: 'bytes.too-large' })
    await expect(writer.close()).resolves.toMatchObject({ state: 'released' })
    await expect(writer.close()).resolves.toMatchObject({ state: 'released' })
    expect(await stage.stagedAcquiredGattCount()).toBe(0)
    expect(await stage.resourceCounters()).toMatchObject({
      acquiredGattTransports: 0,
      pendingGattAcquisitions: 0,
      nativeGattAdmissions: 0
    })
  } finally {
    await connection.release()
    await manager.destroy()
  }
}, 30000)

test('acquired notification source delivers bytes and HUP fails the owned stream', async () => {
  const { manager, connection, stage, characteristic } = await opened()
  try {
    const subscription = await characteristic.acquireNotifications()
    expect(subscription.mtuBytes).toBe(23)
    const iterator = subscription.values[Symbol.asyncIterator]()
    const next = iterator.next()
    await stage.stageAcquiredNotification(new Uint8Array([7, 8]))
    expect([...(await next).value.value.value]).toEqual([7, 8])
    const ended = iterator.next()
    await stage.stageAcquiredHup()
    await expect(ended).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'source-failed', error: { code: 'platform.transport' } }
    })
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
    await expect(subscription.close()).resolves.toMatchObject({ state: 'released' })
    expect(await stage.stagedAcquiredGattCount()).toBe(0)
  } finally {
    await connection.release()
    await manager.destroy()
  }
}, 30000)

test('optional absence, acquisition conflicts and cancelled backpressure never fall back to ordinary writes', async () => {
  const { manager, connection, stage, characteristic } = await opened()
  try {
    await stage.stageAcquiredGatt('fd-peer', { write: false, notify: false, mtu: 23 })
    await expect(characteristic.acquireWrite()).rejects.toMatchObject({ code: 'capability.unsupported' })
    await stage.stageAcquiredGatt('fd-peer', { write: true, notify: true, mtu: 23 })
    const writer = await characteristic.acquireWrite()
    await expect(characteristic.acquireWrite()).rejects.toMatchObject({ code: 'ownership.denied' })
    await expect(characteristic.write(new Uint8Array([3]))).rejects.toMatchObject({ code: 'ownership.denied' })
    await stage.stageAcquiredBackpressure(true)
    const abort = new AbortController()
    const pending = writer.write(new Uint8Array([1]), { signal: abort.signal, timeoutMs: 5000 })
    abort.abort()
    await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
    expect(await stage.stagedAcquiredWriteValues()).toEqual([])
    await writer.close()
    expect((await stage.stagedRadioCalls()).filter(call => call === 'write')).toEqual([])
  } finally {
    await connection.release()
    await manager.destroy()
  }
}, 30000)

test('acquired notifications and ordinary subscriptions refuse conflicting ownership in either order', async () => {
  const { manager, connection, characteristic, stage } = await opened()
  try {
    const acquired = await characteristic.acquireNotifications()
    await expect(characteristic.subscribe()).rejects.toMatchObject({ code: 'ownership.denied' })
    await acquired.close()
    const ordinary = await characteristic.subscribe()
    await expect(characteristic.acquireNotifications()).rejects.toMatchObject({ code: 'ownership.denied' })
    await stage.failNextRadioOp('unsubscribe', 'CCCD removal remains owed')
    await expect(ordinary.remove()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(characteristic.acquireNotifications()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
    await expect(ordinary.remove()).resolves.toMatchObject({ state: 'released' })
    const again = await characteristic.acquireNotifications()
    await again.close()
  } finally {
    await connection.release()
    await manager.destroy()
  }
}, 30000)

test('connection release closes acquired descendants and a changed database rejects old transports', async () => {
  const { manager, connection, stage, database, characteristic } = await opened()
  try {
    const writer = await characteristic.acquireWrite()
    const notifications = await characteristic.acquireNotifications()
    const iterator = notifications.values[Symbol.asyncIterator]()
    const waiting = iterator.next()
    const changes = database.changed[Symbol.asyncIterator]()
    const changed = h.nextValue(changes, 5000)
    await stage.stageServicesChanged('fd-peer')
    await expect(waiting).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'source-failed', error: { code: 'gatt.stale-handle' } }
    })
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
    await expect(writer.write(new Uint8Array([1]))).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    expect(await changed).toMatchObject({ reason: 'service-changed' })
    await changes.return?.()
    await writer.close()
    await notifications.close()
    expect(await stage.stagedAcquiredGattCount()).toBe(0)
    const fresh = (await connection.discover()).characteristic('180d', '2a37')
    const nextWriter = await fresh.acquireWrite()
    const nextNotifications = await fresh.acquireNotifications()
    await connection.release()
    expect(await stage.stagedAcquiredGattCount()).toBe(0)
    await expect(nextWriter.close()).resolves.toMatchObject({ state: 'released' })
    await expect(nextNotifications.close()).resolves.toMatchObject({ state: 'released' })
  } finally {
    await connection.release()
    await manager.destroy()
  }
}, 30000)

test('releasing one public connection closes its acquired children while a shared connection remains usable', async () => {
  const { manager, connection, stage, peer, characteristic } = await opened()
  const shared = await manager.connect(peer)
  try {
    const writer = await characteristic.acquireWrite()
    const notifications = await characteristic.acquireNotifications()
    const iterator = notifications.values[Symbol.asyncIterator]()
    const waiting = iterator.next()
    await connection.release()
    expect(await stage.stagedAcquiredGattCount()).toBe(0)
    await waiting
    await expect(writer.close()).resolves.toMatchObject({ state: 'released' })
    const fresh = (await shared.discover()).characteristic('180d', '2a37')
    const sharedWriter = await fresh.acquireWrite()
    await sharedWriter.write(new Uint8Array([2]))
    expect((await stage.stagedAcquiredWriteValues()).map(value => [...value])).toEqual([[2]])
    await sharedWriter.close()
  } finally {
    await shared.release()
    await connection.release()
    await manager.destroy()
  }
}, 30000)
