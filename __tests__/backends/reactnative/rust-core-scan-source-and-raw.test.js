const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

test.each([true, false])('public raw opt-in is owned, truthful, and optional: %s', async includeRawAdvertisement => {
  const h = rustCoreHarness({ platform: 'android' })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h, { now: () => 20000 }))
  const manager = await createPublicBleManager(internal, () => 20000)
  const scan = await manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement } })
  const iterator = scan.observations[Symbol.asyncIterator]()
  try {
    let pending = iterator.next()
    h.native.emitAdvertisement(DEFAULT_PEER, { rawRecordB64: 'AgEG', sourceTimestampMs: 100, observedAtMs: 19000 })
    const received = (await pending).value
    expect(received).toEqual(expect.objectContaining({ kind: 'value' }))
    const first = received.value
    expect(first.observedAtMonotonicMs).toBe(20000)
    if (includeRawAdvertisement) {
      expect(first.rawAdvertisement).toEqual({
        state: 'present',
        provenance: 'observed',
        value: new Uint8Array([2, 1, 6])
      })
      first.rawAdvertisement.value[2] = 99
    } else expect(first).not.toHaveProperty('rawAdvertisement')
    pending = iterator.next()
    h.native.emitAdvertisement(DEFAULT_PEER, { rawRecordB64: 'AgEG', sourceTimestampMs: 19000, observedAtMs: 19001 })
    const second = (await pending).value.value
    if (includeRawAdvertisement) expect([...second.rawAdvertisement.value]).toEqual([2, 1, 6])
    pending = iterator.next()
    h.native.emitAdvertisement(DEFAULT_PEER, { rawRecordB64: null })
    const absent = (await pending).value.value
    if (includeRawAdvertisement) expect(absent.rawAdvertisement).toMatchObject({ state: 'absent' })
    expect(h.native.opsInvoked('scan.start')).toHaveLength(1)
  } finally {
    await iterator.return()
    await scan.stop()
    await manager.destroy()
  }
})

test.each([100, 19000, null])(
  'mobile preserves platform capture or backend owner time separately from receipt: %s',
  async capture => {
    const h = rustCoreHarness({ platform: 'android' })
    const internal = await createReactNativeBleManagerWithEnvironment(environment(h, { now: () => 20000 }))
    const { scanOptions } = require('../../../test-support/react-native/rust-core-harness')
    const scan = await internal.scan(scanOptions({ timestampPolicy: 'source-then-receipt' }))
    const iterator = scan.observations[Symbol.asyncIterator]()
    try {
      const pending = iterator.next()
      h.native.emitAdvertisement(DEFAULT_PEER, { sourceTimestampMs: capture, observedAtMs: 19000 })
      const item = (await pending).value.value
      expect(item.receivedAtMonotonicMs).toBe(20000)
      expect(item.sourceTimestamp).toMatchObject({
        state: 'present',
        value: { monotonicMs: capture ?? 19000, origin: capture === null ? 'backend' : 'platform' }
      })
    } finally {
      await iterator.return()
      await scan.stop()
      await internal.destroy()
    }
  }
)

test('raw opt-in charges retained bytes against the public stream budget', async () => {
  const h = rustCoreHarness({ platform: 'android' })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const manager = await createPublicBleManager(internal, () => 1000)
  const scan = await manager.scan({
    duplicates: 'all',
    observation: { includeRawAdvertisement: true },
    delivery: { preset: 'custom', budget: { itemCapacity: 8, byteCapacity: 256, reservedControlCapacity: 2, overflowPolicy: 'error' } }
  })
  const iterator = scan.observations[Symbol.asyncIterator]()
  try {
    // Start the pump and drain its first value. The budget bounds retained
    // records, so the oversized record must arrive while no next() awaits it.
    const initial = iterator.next()
    h.native.emitAdvertisement(DEFAULT_PEER, { rawRecordB64: 'AgEG' })
    await expect(initial).resolves.toMatchObject({ value: { kind: 'value' } })
    h.native.emitAdvertisement(DEFAULT_PEER, { rawRecordB64: Buffer.alloc(1024, 1).toString('base64') })
    await new Promise(resolve => setImmediate(resolve))
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'overflow' } })
  } finally {
    await iterator.return()
    await scan.stop()
    await manager.destroy()
  }
})

test('raw bytes belong independently to each public subscriber', async () => {
  const h = rustCoreHarness({ platform: 'android' })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const manager = await createPublicBleManager(internal, () => 1000)
  const scan = await manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  const a = scan.observations[Symbol.asyncIterator]()
  const b = scan.observations[Symbol.asyncIterator]()
  try {
    const waitingA = a.next(),
      waitingB = b.next()
    h.native.emitAdvertisement(DEFAULT_PEER, { rawRecordB64: 'AgEG' })
    const first = (await waitingA).value.value.rawAdvertisement.value
    first[2] = 99
    expect([...(await waitingB).value.value.rawAdvertisement.value]).toEqual([2, 1, 6])
  } finally {
    await a.return()
    await b.return()
    await scan.stop()
    await manager.destroy()
  }
})

test.each(['manufacturer', 'name'])('Android public filtering retains newer captured %s facts across an out-of-order OS batch', async fact => {
  const h = rustCoreHarness({ platform: 'android' })
  let now = 20000
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h, { now: () => now }))
  const manager = await createPublicBleManager(internal, () => now)
  const scan = await manager.scan({ duplicates: 'all', query: { anyOf: [{
    names: { exact: ['Capture target'] },
    manufacturerData: { all: [{ companyId: 107, dataPrefix: new Uint8Array([1]) }] }
  }] } })
  const iterator = scan.observations[Symbol.asyncIterator]()
  try {
    const next = iterator.next()
    const packets = fact === 'manufacturer'
      ? [[200, null, [2]], [100, null, [1]], [300, 'Capture target', null], [400, null, [1]]]
      : [[200, 'Other capture', null], [100, 'Capture target', null], [300, null, [1]], [400, 'Capture target', null]]
    for (const [capture, name, bytes] of packets) {
      now++
      h.native.emitAdvertisement(DEFAULT_PEER, { localName: name,
        manufacturerData: bytes === null ? null : [{ companyId: 107, payloadB64: Buffer.from(bytes).toString('base64') }],
        sourceTimestampMs: capture, observedAtMs: now })
      await new Promise(resolve => setImmediate(resolve))
    }
    const item = (await next).value
    if (item.kind === 'terminal') throw new Error(JSON.stringify(item))
    expect(item).toMatchObject({ kind: 'value', value: { localName: 'Capture target' } })
    expect(item.value.observedAtMonotonicMs).toBe(20004)
    expect([...item.value.manufacturerData[0].data]).toEqual([1])
  } finally {
    await iterator.return()
    await scan.stop()
    await manager.destroy()
  }
})
