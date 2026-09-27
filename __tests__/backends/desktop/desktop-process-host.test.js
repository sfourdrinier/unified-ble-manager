'use strict'

const { createDesktopProcessHostFromBackend } = require('../../../src/desktop-process-host')
const h = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

const ids = {
  corebluetooth: '00e2ce71-3ba4-6569-e3de-3081ce0c95fb',
  bluez: 'hci0/dev_AA_BB_CC_DD_EE_FF',
  winrt: 'AA:BB:CC:DD:EE:FF'
}
const declaration = peerId => ({
  onAppearance: 'native',
  peerId,
  resubscribe: [
    {
      serviceUuid: h.HRM_SERVICE,
      serviceOccurrence: 1,
      characteristicUuid: h.HRM_MEASUREMENT,
      characteristicOccurrence: 1
    }
  ]
})

async function observed(manager, stage, peerId) {
  const scan = await manager.scan()
  const iterator = scan.observations[Symbol.asyncIterator]()
  const pending = h.nextValue(iterator, 5000)
  await stage.stageAdvertisement({ peerId, rssi: -50, localName: 'Shared native owner' })
  const value = await pending
  await iterator.return?.()
  expect(await scan.stop()).toMatchObject({ state: 'released' })
  return value.peer
}

test.each(['bluez', 'corebluetooth', 'winrt'])(
  '%s native data survives independent foreground destruction',
  async platform => {
    const { backend, stage, harness } = await h.openBackend(platform)
    const host = await createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
    try {
      const manager = await host.createManager()
      const peerId = ids[platform]
      await stage.stageServices(peerId, h.hrmServices())
      const peer = await observed(manager, stage, peerId)
      const connection = await manager.connect(peer)
      const database = await connection.discover()
      const subscription = await database.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT).subscribe()
      const values = subscription.values[Symbol.asyncIterator]()
      const first = h.nextValue(values, 5000)
      await host.continuation.execute(declaration(peerId))
      await stage.stageNotification({
        peerId,
        serviceUuid: h.HRM_SERVICE,
        characteristicUuid: h.HRM_MEASUREMENT,
        value: Buffer.from([0, 72])
      })
      expect((await first).value).toEqual(new Uint8Array([0, 72]))
      await values.return?.()
      const closeCount = harness.calls.filter(([name]) => name === 'close').length
      expect(await manager.destroy()).toMatchObject({ state: 'released' })
      expect(harness.calls.filter(([name]) => name === 'close')).toHaveLength(closeCount)
      await stage.stageNotification({
        peerId,
        serviceUuid: h.HRM_SERVICE,
        characteristicUuid: h.HRM_MEASUREMENT,
        value: Buffer.from([0, 73])
      })
      const acknowledgements = harness.calls.filter(([name]) => name === 'continuationAcknowledgeClaim').length
      const prepared = await host.continuationAccess.prepareClaim(256, 65536)
      expect(await host.continuationAccess.prepareClaim(256, 65536)).toEqual(prepared)
      expect(harness.calls.filter(([name]) => name === 'continuationAcknowledgeClaim')).toHaveLength(acknowledgements)
      const backlog = await host.continuation.claim()
      expect(backlog.disposed).toBe(true)
      expect(backlog.values.map(item => [...item.value])).toEqual(
        expect.arrayContaining([
          [0, 72],
          [0, 73]
        ])
      )
    } finally {
      expect(await host.destroy()).toMatchObject({ state: 'released' })
    }
  }
)

test('native cleanup refusal remains retryable and post-close backlog stays reachable', async () => {
  const { backend, stage } = await h.openBackend('corebluetooth')
  const host = await createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const peerId = ids.corebluetooth
  await stage.stageServices(peerId, h.hrmServices())
  await host.continuation.execute(declaration(peerId))
  await stage.stageNotification({
    peerId,
    serviceUuid: h.HRM_SERVICE,
    characteristicUuid: h.HRM_MEASUREMENT,
    value: Buffer.from([0, 74])
  })
  await stage.failNextRadioOp('disconnect', 'retained native disconnect refusal')
  const failed = await host.destroy()
  expect(failed.state).toBe('release-failed')
  expect(failed.failures.length).toBeGreaterThan(0)
  await expect(host.createManager()).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
  expect(await host.destroy()).toEqual({ state: 'released', failures: [] })
  const backlog = await host.continuation.claim()
  expect(backlog.values.map(item => [...item.value])).toContainEqual([0, 74])
  expect(backlog.disposed).toBe(true)
})

test('successful host close preserves positive native backlog for explicit later claim', async () => {
  const { backend, stage } = await h.openBackend('corebluetooth')
  const host = await createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const peerId = ids.corebluetooth
  await stage.stageServices(peerId, h.hrmServices())
  await host.continuation.execute(declaration(peerId))
  await stage.stageNotification({
    peerId,
    serviceUuid: h.HRM_SERVICE,
    characteristicUuid: h.HRM_MEASUREMENT,
    value: Buffer.from([0, 75])
  })
  expect(await host.destroy()).toEqual({ state: 'released', failures: [] })
  const backlog = await host.continuation.claim()
  expect(backlog.values.map(item => [...item.value])).toContainEqual([0, 75])
  expect(backlog.disposed).toBe(true)
})

test('initialization cleanup failure exposes exact native retry owner', async () => {
  const { backend, stage } = await h.openBackend('corebluetooth')
  await stage.failNextRadioOp('stop-scan', 'retained initialization cleanup')
  // Actual native shutdown refusal requires a live physical scan.
  const scan = await stage.startScan({ owner: 'initialization-fixture' })
  expect(scan.operationId).toBeTruthy()
  let failure
  try {
    await createDesktopProcessHostFromBackend(backend, {
      now: () => performance.now(),
      randomBytes: () => new Uint8Array(0)
    })
  } catch (error) {
    failure = error
  }
  expect(failure).toMatchObject({
    code: 'platform.failure',
    domain: 'cleanup',
    originalCause: expect.any(Error),
    retryCleanup: expect.any(Function)
  })
  expect(await failure.retryCleanup()).toEqual({ state: 'released', failures: [] })
})

test('late public wrapper cannot publish a borrower after process closure', async () => {
  const publicModule = require('../../../src/public/ble-manager')
  const original = publicModule.createPublicBleManager
  let admitted
  const arrived = new Promise(resolve => {
    admitted = resolve
  })
  let complete
  const gate = new Promise(resolve => {
    complete = resolve
  })
  const spy = jest.spyOn(publicModule, 'createPublicBleManager').mockImplementation(async (...args) => {
    const manager = await original(...args)
    admitted()
    await gate
    return manager
  })
  const { backend } = await h.openBackend('corebluetooth')
  const destroyBackend = backend.destroy.bind(backend)
  let nativeClosed
  const nativeClose = new Promise(resolve => {
    nativeClosed = resolve
  })
  jest.spyOn(backend, 'destroy').mockImplementation(async () => {
    const receipt = await destroyBackend()
    nativeClosed(receipt)
    return receipt
  })
  const host = await createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  const pending = host.createManager()
  const rejected = expect(pending).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
  await arrived
  const closing = host.destroy()
  expect(await nativeClose).toMatchObject({ state: 'released' })
  complete()
  await rejected
  expect(await closing).toEqual({ state: 'released', failures: [] })
  spy.mockRestore()
})

test.each(['bluez', 'corebluetooth', 'winrt'])('%s process owner survives all public borrowers', async platform => {
  const { backend, stage, harness } = await h.openBackend(platform)
  const host = await createDesktopProcessHostFromBackend(backend, { now: () => performance.now() })
  try {
    const a = await host.createManager()
    const b = await host.createManager()
    const before = harness.calls.filter(([name]) => name === 'close').length
    expect(await a.destroy()).toEqual({ state: 'released', failures: [] })
    expect(await b.destroy()).toEqual({ state: 'released', failures: [] })
    expect(harness.calls.filter(([name]) => name === 'close')).toHaveLength(before)
    const c = await host.createManager()
    expect(await c.adapter.state()).toBeDefined()
    expect(await c.destroy()).toMatchObject({ state: 'released' })
    expect(stage.dispatchCounters().connect).toBe(0)
  } finally {
    expect(await host.destroy()).toEqual({ state: 'released', failures: [] })
  }
  await expect(host.createManager()).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
  await expect(
    host.continuation.execute({ onAppearance: 'native', peerId: 'peer', resubscribe: [] })
  ).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
})
