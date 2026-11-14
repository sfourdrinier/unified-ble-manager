const { rustCoreHarness, environment, settle, scanOptions } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createBleManager, createManagerOwnershipAuthority, DEFAULT_BLE_MANAGER_OPTIONS } = require('../../../src/manager/ble-manager')
const { opaqueId } = require('../../../src/backend-contract/primitives')
const { connectToDeterministicPeer } = require('../../../src/tck/runner-public-scenario-support')
const { encodeTerminalEvidence } = require('../../../src/core/terminal-evidence-codec')
const { createHash } = require('node:crypto')

async function setup(options = {}) {
  const h = rustCoreHarness()
  if (options.scanEndV2) h.native.negotiateScanEndVersion = async () => JSON.stringify({ version: 2 })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h, { now: options.now ?? (() => 1000) }))
  const attached = internal.attachedBackend
  const core = await createBleManager({ attachedBackend: attached, clientId: opaqueId('accounting-client', 'client', 'test'), managerId: opaqueId('accounting-manager', 'manager', 'test'), ownerMode: 'owning' }, createManagerOwnershipAuthority(attached), { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => 1000, ...options.core })
  return { ...h, internal, core, manager: await createPublicBleManager(core, options.now ?? (() => 1000)) }
}

const constrainedScan = () => scanOptions({ delivery: { itemCapacity: 4, byteCapacity: 128, reservedControlCapacity: 1, overflowPolicy: 'drop-oldest' } })

test('TCK peer acquisition connects through the Android provider using the unmodified fixture', async () => {
  const h = await setup()
  const fixture = { controller: {
    settle: promise => promise,
    perform: async () => h.native.emitAdvertisement(),
    flush: () => settle(100)
  } }
  let connection
  try {
    connection = await connectToDeterministicPeer(h.core, fixture, { id: 'android-peer-setup' })
    expect(h.native.opsInvoked('connection.connect')).toHaveLength(1)
  } finally {
    if (connection) await connection.release()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('256-byte provider scan admits the unmodified fixture', async () => {
  const h = await setup()
  const options = constrainedScan()
  const lease = await h.internal.attachedBackend.backend.scanner.start({ ...options, delivery: { ...options.delivery, byteCapacity: 256 } }, opaqueId('fixture-admission', 'client', 'test'))
  try {
    h.native.emitAdvertisement()
    await settle(100)
    expect((await lease.observations[Symbol.asyncIterator]().next()).value).toMatchObject({ kind: 'value' })
  } finally {
    await lease.stop()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('128-byte provider scan reports the 136-byte fixture overflow and stays active', async () => {
  const h = await setup()
  const lease = await h.internal.attachedBackend.backend.scanner.start(constrainedScan(), opaqueId('fixture-overflow', 'client', 'test'))
  try {
    h.native.emitAdvertisement()
    await settle(100)
    const iterator = lease.observations[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'overflow', droppedItems: 1, droppedBytes: 136, replacedItems: 0 })
    expect(h.native.liveSessions()[0].scans.size).toBe(1)
    h.native.emitAdvertisement(undefined, { localName: 'small', serviceUuids: null, manufacturerData: null })
    await settle(100)
    expect((await iterator.next()).value).toMatchObject({ kind: 'value', value: { localName: { state: 'present', value: 'small' } } })
  } finally {
    await lease.stop()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('oversized provider advertisement leaves an already accepted smaller value readable', async () => {
  const h = await setup()
  const lease = await h.internal.attachedBackend.backend.scanner.start(constrainedScan(), opaqueId('accepted-before-oversize', 'client', 'test'))
  try {
    h.native.emitAdvertisement(undefined, { localName: 'small', serviceUuids: null, manufacturerData: null })
    await settle(100)
    const consumer = lease.observations.claimConsumer()
    const claim = consumer.claim()
    expect(claim).not.toBeNull()
    expect(claim.item).toMatchObject({ kind: 'value', value: { localName: { state: 'present', value: 'small' } } })
    claim.rollback()
    consumer.close()
    h.native.emitAdvertisement()
    await settle(100)
    expect(h.native.liveSessions()[0].scans.size).toBe(1)
    await lease.stop()
    const iterator = lease.observations[Symbol.asyncIterator]()
    const notice = (await iterator.next()).value
    const retained = (await iterator.next()).value
    expect([notice, retained]).toEqual([
      expect.objectContaining({ kind: 'overflow', droppedItems: 1, droppedBytes: 136, replacedItems: 0 }),
      expect.objectContaining({ kind: 'value', value: expect.objectContaining({ localName: expect.objectContaining({ state: 'present', value: 'small' }) }) })
    ])
  } finally {
    await lease.stop()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('backend lossless overflow retains the accepted prefix and reports the rejected record', async () => {
  const h = await setup()
  const lease = await h.internal.attachedBackend.backend.scanner.start(scanOptions({ delivery: { itemCapacity: 2, byteCapacity: 65536, reservedControlCapacity: 4, overflowPolicy: 'error' } }), opaqueId('direct-accounting', 'client', 'test'))
  for (let i = 0; i < 3; i++) { h.native.emitAdvertisement(); await settle(50) }
  const it = lease.observations[Symbol.asyncIterator]()
  expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 1 })
  await h.manager.destroy()
  await h.internal.destroy()
})

test('public stop preserves an unopened shared core scan and releases its quota after reading', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  h.native.emitAdvertisement(undefined, { rawRecordB64: 'AQIDBA==' })
  await settle(100)
  expect(h.core.core.aggregateQuota.retainedPayloadBytes()).toBeGreaterThan(0)
  await scan.stop()
  const it = scan.observations[Symbol.asyncIterator]()
  expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  expect(h.core.core.aggregateQuota.retainedPayloadBytes()).toBe(0)
  await h.manager.destroy()
  await h.internal.destroy()
})

test('backend diagnostics retain accepted warning after malformed drain', async () => {
  const h = await setup()
  const it = h.internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  const session = h.native.liveSessions()[0]
  h.native.push(session, { t: 'scan-end', operationId: 'unknown', reason: 'owner-released' })
  await settle(80)
  h.native.push(session, { t: 'invalid-record' })
  await settle(80)
  expect((await it.next()).value).toMatchObject({ kind: 'value', value: { code: 'unmatched-scan-end' } })
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'source-failed' })
  await h.manager.destroy()
  await h.internal.destroy()
})

test('advertisement routed while native stop is pending remains in the accepted prefix', async () => {
  const h = await setup()
  const lease = await h.internal.attachedBackend.backend.scanner.start(scanOptions(), opaqueId('pending-stop', 'client', 'test'))
  h.native.hold('scan.stop')
  const stopping = lease.stop()
  await settle(50)
  h.native.emitAdvertisement()
  await settle(80)
  h.native.release('scan.stop')
  await stopping
  const it = lease.observations[Symbol.asyncIterator]()
  expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await h.manager.destroy()
  await h.internal.destroy()
})

test('a failed native stop leaves its observation source live until a retry releases it', async () => {
  const h = await setup()
  const lease = await h.internal.attachedBackend.backend.scanner.start(scanOptions(), opaqueId('failed-stop', 'client', 'test'))
  h.native.failNext('scan.stop', 'platform.failure', 'platform', 'scan.stop', 'radio busy')

  const refused = await lease.stop()

  expect(refused).toMatchObject({ state: 'release-failed', failures: [{ error: { code: 'platform.failure' } }] })
  expect(lease.observations.isTerminal()).toBe(false)
  h.native.emitAdvertisement(undefined, { localName: 'still-live', serviceUuids: null, manufacturerData: null })
  await settle(80)
  const iterator = lease.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({
    kind: 'value',
    value: { localName: { state: 'present', value: 'still-live' } }
  })

  expect(await lease.stop()).toEqual({ state: 'released', failures: [] })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await h.manager.destroy()
  await h.internal.destroy()
})

test('source overflow counters pass through shared core and public terminal', async () => {
  const h = await setup()
  const backend = h.internal.attachedBackend.backend
  const scanner = backend.scanner
  let release
  const gate = new Promise(resolve => { release = resolve })
  backend.scanner = { ...scanner, start: async (...args) => {
    const lease = await scanner.start(...args)
    return { ...lease, observations: { ...lease.observations, [Symbol.asyncIterator]() {
      const source = lease.observations[Symbol.asyncIterator]()
      return { next: async () => { await gate; return source.next() }, return: () => source.return(), [Symbol.asyncIterator]() { return this } }
    } } }
  } }
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all' })
  for (let i = 0; i < 65; i++) { h.native.emitAdvertisement(); await settle(20) }
  release()
  await settle(200)
  const it = scan.observations[Symbol.asyncIterator]()
  for (let i = 0; i < 64; i++) expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 1 })
  await h.manager.destroy()
  await h.internal.destroy()
})

test('slow public lossless view keeps its accepted prefix on overflow', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all' })
  const it = scan.observations[Symbol.asyncIterator]()
  for (let i = 0; i < 65; i++) { h.native.emitAdvertisement(); await settle(30) }
  for (let i = 0; i < 64; i++) expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 1 })
  await settle(100)
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThan(0)
  await scan.observations.close()
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
  await h.manager.destroy()
  await h.internal.destroy()
})

test('shared aggregate quota remains charged for a slow public observation view', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  const it = scan.observations[Symbol.asyncIterator]()
  h.native.emitAdvertisement(undefined, { rawRecordB64: 'AQIDBA==' })
  await settle(100)
  await scan.stop()
  await settle(100)
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThan(0)
  expect((await it.next()).value.kind).toBe('value')
  expect((await it.next()).value.kind).toBe('terminal')
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThan(0)
  await scan.observations.close()
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
  await h.manager.destroy()
  await h.internal.destroy()
})

test('closing a stopped slow public view releases its retained quota and counts discarded values', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  const it = scan.observations[Symbol.asyncIterator]()
  h.native.emitAdvertisement(undefined, { rawRecordB64: 'AQIDBA==' })
  await settle(100)
  await scan.stop()
  await settle(100)
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThan(0)
  await scan.observations.close()
  // The closed view still owns its unread terminal; release its control
  // reservation only after the iterator receives it.
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThan(0)
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'closed', droppedItems: 1 })
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
  await h.manager.destroy()
  await h.internal.destroy()
})

test('manager destruction releases a stopped unopened view still charged to shared quota', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  h.native.emitAdvertisement(undefined, { rawRecordB64: 'AQIDBA==' })
  await settle(100)
  await scan.stop()
  await settle(100)
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThan(0)
  await h.manager.destroy()
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
  await h.internal.destroy()
})

test('shared core separates bounded queue and terminal-evidence accounting until the public terminal is read', async () => {
  const h = await setup({ scanEndV2: true, core: { terminalEvidence: { maximumCanonicalBytes: 16384 } } })
  const scan = await h.manager.scan({ duplicates: 'all' })
  h.native.emitAdvertisement()
  await settle(100)
  const session = h.native.liveSessions()[0]
  const membership = [...session.scans.keys()][0]
  const scope = h.native.opsInvoked('terminal-evidence.reserve')[0].scope
  const error = { code: 'adapter.powered-off', domain: 'adapter', operation: 'scan.start', retryability: 'never', detail: 'failure '.repeat(1000), platform: null }
  const canonicalBytes = encodeTerminalEvidence({ kind: 'terminal', reason: 'source-failed', droppedItems: 0, droppedBytes: 0, replacedItems: 0, error }, scope)
  const digest = createHash('sha256').update(canonicalBytes).digest('hex')
  const reservationId = [...h.native.terminalEvidenceReservations.keys()][0]
  const committed = JSON.parse(await h.native.invoke(session.id, 'terminal-evidence.commit', JSON.stringify({ reservationId, canonicalBytesBase64: Buffer.from(canonicalBytes).toString('base64'), digest })))
  expect(committed.ok).toBe(true)
  session.scans.delete(membership)
  h.native.push(session, { t: 'scan-end', operationId: membership, startOperationId: h.native.opsInvoked('scan.start')[0].operationId, reason: 'source-failed', version: 2, source: 'adapter', generation: '1', error, evidence: { producer: 'scan', kind: committed.value.kind, ref: committed.value.ref, byteLength: committed.value.byteLength, digest: committed.value.digest, formatVersion: committed.value.formatVersion, durability: committed.value.durability, scope } })
  await settle(150)
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(1024)
  const retainedBeforeRead = h.core.core.aggregateQuota.retainedByteUsage()
  expect(retainedBeforeRead.queueBytes).toBeLessThanOrEqual(1024)
  expect(retainedBeforeRead.terminalEvidenceBytes).toBeGreaterThan(7000)
  expect(retainedBeforeRead.totalBytes).toBeGreaterThan(7000)
  const it = scan.observations[Symbol.asyncIterator]()
  expect((await it.next()).value.kind).toBe('value')
  expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(1024)
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'source-failed', error: { code: 'adapter.powered-off' } })
  expect(h.native.opsInvoked('terminal-evidence.read')).toEqual([
    { ref: committed.value.ref, maximumBytes: 16384 },
    { ref: committed.value.ref, maximumBytes: 16384 }
  ])
  expect(h.core.core.aggregateQuota.retainedByteUsage().terminalEvidenceBytes).toBe(0)
  await scan.observations.close()
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
  expect(h.core.core.aggregateQuota.retainedByteUsage().terminalEvidenceBytes).toBe(0)
  await h.manager.destroy()
  await h.internal.destroy()
})

test('closing an unopened public view reports the core values explicitly discarded', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all' })
  h.native.emitAdvertisement()
  await settle(100)
  await scan.stop()
  await scan.observations.close()
  const it = scan.observations[Symbol.asyncIterator]()
  expect((await it.next()).value).toMatchObject({ kind: 'terminal', reason: 'closed', droppedItems: 1 })
  await h.manager.destroy()
  await h.internal.destroy()
})

test('shared core aggregate quota overflow retains its accepted prefix and reports the rejected observation', async () => {
  const h = await setup({ core: { maximumAggregateRetainedBytes: 2000 } })
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all' })
  for (let i = 0; i < 12; i++) { h.native.emitAdvertisement(); await settle(30) }
  await settle(100)
  const it = scan.observations[Symbol.asyncIterator]()
  let values = 0
  let terminal
  for (;;) {
    const result = await it.next()
    if (result.done) break
    if (result.value.kind === 'value') values++
    if (result.value.kind === 'terminal') { terminal = result.value; break }
  }
  expect(values).toBeGreaterThan(0)
  expect(terminal).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 1 })
  expect(terminal.droppedBytes).toBeGreaterThan(0)
  await scan.observations.close()
  await h.manager.destroy()
  await h.internal.destroy()
})

test('large retained scan metadata does not consume the per-delivery byte budget', async () => {
  const h = await setup({ core: { maximumAggregateRetainedBytes: 20000 } })
  const name = 'x'.repeat(150)
  const scan = await h.manager.scan({ duplicates: 'all', delivery: { preset: 'custom', budget: {
    itemCapacity: 2, byteCapacity: 256, reservedControlCapacity: 1, overflowPolicy: 'error'
  } } })
  try {
    h.native.emitAdvertisement(undefined, { localName: name, serviceUuids: null, manufacturerData: null })
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThanOrEqual(name.length * 2)
    const iterator = scan.observations[Symbol.asyncIterator]()
    expect((await iterator.next()).value.value.localName).toBe(name)
    await iterator.return()
  } finally {
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('public fan-out never grows the shared quota beyond its ceiling when draining an accepted prefix', async () => {
  const maximumAggregateRetainedBytes = 2000
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all' })
  for (let i = 0; i < 12; i++) { h.native.emitAdvertisement(undefined, { rssi: -90 + i }); await settle(30) }
  const first = scan.observations[Symbol.asyncIterator]()
  const second = scan.observations[Symbol.asyncIterator]()
  try {
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    const received = [[], []]
    for (;;) {
      const pair = await Promise.all([first.next(), second.next()])
      for (let i = 0; i < pair.length; i++) {
        if (!pair[i].done && pair[i].value.kind === 'value') received[i].push(pair[i].value.value.rssi)
      }
      expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
      if (pair.every(item => !item.done && item.value.kind === 'terminal')) break
    }
    expect(received[0].length).toBeGreaterThan(0)
    expect(received[0]).toEqual(Array.from({ length: received[0].length }, (_, i) => -90 + i))
    expect(received[1]).toEqual(received[0])
  } finally {
    await first.return()
    await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('event fan-out stays within the aggregate ceiling while retaining the accepted prefix', async () => {
  const maximumAggregateRetainedBytes = 2000
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all' })
  for (let i = 0; i < 12; i++) { h.native.emitAdvertisement(undefined, { rssi: -90 + i }); await settle(30) }
  const first = scan.events[Symbol.asyncIterator]()
  const second = scan.events[Symbol.asyncIterator]()
  try {
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    const received = [[], []]
    for (;;) {
      const pair = await Promise.allSettled([first.next(), second.next()])
      for (let i = 0; i < pair.length; i++) {
        if (pair[i].status === 'fulfilled' && !pair[i].value.done && pair[i].value.value.kind === 'observed') {
          received[i].push(pair[i].value.value.peer.rssi)
        }
      }
      expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
      if (pair.some(item => item.status === 'rejected')) {
        expect(pair).toEqual(pair.map(() => expect.objectContaining({
          status: 'rejected', reason: expect.objectContaining({ code: 'stream.overflow' })
        })))
        break
      }
      expect(pair.every(item => item.status === 'fulfilled' && !item.value.done)).toBe(true)
    }
    expect(received[0].length).toBeGreaterThan(0)
    expect(received[0]).toEqual(Array.from({ length: received[0].length }, (_, i) => -90 + i))
    expect(received[1]).toEqual(received[0])
  } finally {
    await first.return()
    await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('public split-packet evidence remains charged after the observation is delivered', async () => {
  const h = await setup({ core: { maximumAggregateRetainedBytes: 16384 } })
  const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{
    names: { exact: ['Split target'] }, manufacturerData: { all: [{ companyId: 107 }] }
  }] } })
  const iterator = scan.observations[Symbol.asyncIterator]()
  const payload = Buffer.alloc(2048, 7)
  try {
    h.native.emitAdvertisement(undefined, { localName: 'Split target', manufacturerData: [{ companyId: 107, payloadB64: payload.toString('base64') }] })
    const item = await iterator.next()
    expect(item.value.kind).toBe('value')
    expect(Buffer.from(item.value.value.manufacturerData[0].data)).toEqual(payload)
    await settle(100)
    // The evidence session still holds the manufacturer fact for a later
    // split packet even though the subscriber has consumed this occurrence.
    const retainedAfterDelivery = h.core.core.aggregateQuota.retainedBytes()
    h.native.emitAdvertisement(undefined, { localName: 'Split target', manufacturerData: null })
    const merged = await iterator.next()
    expect(Buffer.from(merged.value.value.manufacturerData[0].data)).toEqual(payload)
    expect(retainedAfterDelivery).toBeGreaterThanOrEqual(payload.length)
  } finally {
    await iterator.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('transferred split evidence reports each backing buffer once across readers and release orders', async () => {
  for (const releaseCacheFirst of [false, true]) {
    const h = await setup({ core: { maximumAggregateRetainedBytes: 20000 } })
    const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{
      names: { exact: ['target'] }, manufacturerData: { all: [{ companyId: 107 }] }
    }] } })
    const first = scan.observations[Symbol.asyncIterator]()
    const second = scan.observations[Symbol.asyncIterator]()
    const bytes = () => h.core.core.localResourceCounters().retainedByteBuffers
    try {
      for (const [peer, value] of [['peer-a', 1], ['peer-b', 2]]) {
        h.native.emitAdvertisement(peer, { localName: null, serviceUuids: null, manufacturerData: [
          { companyId: 107, payloadB64: Buffer.alloc(1024, value).toString('base64') }
        ] })
        await settle(60)
        h.native.emitAdvertisement(peer, { localName: 'target', serviceUuids: null, manufacturerData: null })
        await settle(60)
      }
      // Source claims have drained; cache and both queued readers own the same buffers.
      expect([...h.core.core.scans.values()][0].stream.values).toHaveLength(0)
      expect(bytes()).toBe(2048)
      if (releaseCacheFirst) await scan.stop()
      expect(bytes()).toBe(2048)
      for (const reader of [first, second]) {
        expect((await reader.next()).value.value.manufacturerData[0].data).toHaveLength(1024)
        expect((await reader.next()).value.value.manufacturerData[0].data).toHaveLength(1024)
      }
      expect(bytes()).toBe(releaseCacheFirst ? 0 : 2048)
      if (!releaseCacheFirst) await scan.stop()
      expect(bytes()).toBe(0)
    } finally {
      await first.return()
      await second.return()
      await scan.observations.close()
      await h.manager.destroy()
      await h.internal.destroy()
    }
  }
})

test('unrelated peer evidence does not prevent fitting merged observations from advancing', async () => {
  const maximumAggregateRetainedBytes = 7000
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{
    names: { exact: ['target'] }, manufacturerData: { all: [{ companyId: 107 }] }
  }] } })
  const first = scan.observations[Symbol.asyncIterator]()
  const second = scan.observations[Symbol.asyncIterator]()
  try {
    for (let i = 0; i < 5; i++) {
      h.native.emitAdvertisement(`unrelated-${i}`, { localName: `unrelated-${i}`, serviceUuids: null, manufacturerData: null })
      await settle(60)
    }
    for (const [peer, byte] of [['target-a', 7], ['target-b', 8]]) {
      h.native.emitAdvertisement(peer, { localName: null, serviceUuids: null, manufacturerData: [
        { companyId: 107, payloadB64: Buffer.alloc(256, byte).toString('base64') }
      ] })
      await settle(60)
      h.native.emitAdvertisement(peer, { localName: 'target', serviceUuids: null, manufacturerData: null })
      await settle(60)
    }
    expect([...h.core.core.scans.values()][0].stream.values).toHaveLength(0)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    let delivered = false
    const readBoth = async reader => [(await reader.next()).value.value, (await reader.next()).value.value]
    const read = Promise.all([readBoth(first), readBoth(second)]).then(items => { delivered = true; return items })
    await settle(100)
    expect(delivered).toBe(true)
    const [a, b] = await read
    expect(a.map(value => value.manufacturerData[0].data[0])).toEqual([7, 8])
    expect(b.map(value => value.manufacturerData[0].data[0])).toEqual([7, 8])
    expect(a[0].peer.id).not.toBe(a[1].peer.id)
    expect(b.map(value => value.peer.id)).toEqual(a.map(value => value.peer.id))
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
  } finally {
    await first.return()
    await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('shared cached evidence and queued fanout charge one payload backing buffer', async () => {
  async function retainedFor(size) {
    const h = await setup({ core: { maximumAggregateRetainedBytes: 20000 } })
    const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{ names: { exact: ['target'] } }] } })
    const reader = scan.observations[Symbol.asyncIterator]()
    try {
      h.native.emitAdvertisement(undefined, { localName: 'target', manufacturerData: [
        { companyId: 107, payloadB64: Buffer.alloc(size, 7).toString('base64') }
      ] })
      await settle(100)
      return h.core.core.aggregateQuota.retainedBytes()
    } finally {
      await reader.return()
      await scan.observations.close()
      await h.manager.destroy()
      await h.internal.destroy()
    }
  }
  const small = await retainedFor(1)
  const large = await retainedFor(1024)
  expect(large - small).toBe(1023)
})

test('cache replacement and queued readers retain each backing allocation until its last owner releases', async () => {
  async function snapshots(size) {
    const h = await setup({ core: { maximumAggregateRetainedBytes: 30000 } })
    const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{ names: { exact: ['target'] } }] } })
    const first = scan.observations[Symbol.asyncIterator]()
    const second = scan.observations[Symbol.asyncIterator]()
    const advertise = byte => h.native.emitAdvertisement(undefined, { localName: 'target', manufacturerData: [
      { companyId: 107, payloadB64: Buffer.alloc(size, byte).toString('base64') }
    ] })
    try {
      advertise(1)
      await settle(100)
      const shared = h.core.core.aggregateQuota.retainedBytes()
      await first.next()
      const oneReader = h.core.core.aggregateQuota.retainedBytes()
      advertise(2)
      await settle(100)
      const replaced = h.core.core.aggregateQuota.retainedBytes()
      await second.next()
      const oldReleased = h.core.core.aggregateQuota.retainedBytes()
      await scan.stop()
      const cacheReleased = h.core.core.aggregateQuota.retainedBytes()
      await first.next()
      const lastReader = h.core.core.aggregateQuota.retainedBytes()
      await second.next()
      const allReadersReleased = h.core.core.aggregateQuota.retainedBytes()
      return { shared, oneReader, replaced, oldReleased, cacheReleased, lastReader, allReadersReleased }
    } finally {
      await first.return()
      await second.return()
      await scan.observations.close()
      await h.manager.destroy()
      await h.internal.destroy()
    }
  }
  const small = await snapshots(1)
  const large = await snapshots(1024)
  for (const key of ['shared', 'oneReader', 'oldReleased', 'cacheReleased', 'lastReader']) {
    expect(large[key] - small[key]).toBe(1023)
  }
  expect(large.replaced - small.replaced).toBe(2046)
  expect(large.allReadersReleased - small.allReadersReleased).toBe(0)
})

test('split evidence and both public fanouts are admitted together within the aggregate ceiling', async () => {
  const maximumAggregateRetainedBytes = 4096
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{ names: { exact: ['Split target'] } }] } })
  const first = scan.observations[Symbol.asyncIterator]()
  const second = scan.observations[Symbol.asyncIterator]()
  const events = scan.events[Symbol.asyncIterator]()
  try {
    h.native.emitAdvertisement(undefined, {
      localName: 'Split target', sourceTimestampMs: 1,
      manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, 7).toString('base64') }]
    })
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    const [a, b, event] = await Promise.all([first.next(), second.next(), events.next()])
    expect(a.value.value.manufacturerData[0].data).toHaveLength(1024)
    expect(b.value.value.manufacturerData[0].data).toHaveLength(1024)
    expect(event.value.kind).toBe('observed')
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
  } finally {
    await first.return()
    await second.return()
    await events.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('drop-oldest fanout stays within aggregate quota when replacing full subscriber queues', async () => {
  const maximumAggregateRetainedBytes = 4096
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ delivery: 'latest', duplicates: 'all' })
  const readers = Array.from({ length: 4 }, () => scan.observations[Symbol.asyncIterator]())
  const advertisement = { manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, 7).toString('base64') }] }
  try {
    h.native.emitAdvertisement(undefined, advertisement)
    await settle(100)
    h.native.emitAdvertisement(undefined, advertisement)
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
  } finally {
    for (const reader of readers) await reader.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('drop-newest keeps each full subscriber prefix while drained subscribers and presence advance', async () => {
  const maximumAggregateRetainedBytes = 4096
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ observation: { reportLostAfterMs: 60000 },
    delivery: { preset: 'custom', budget: { itemCapacity: 1, byteCapacity: 4096, reservedControlCapacity: 2, overflowPolicy: 'drop-newest' } } })
  const slow = scan.observations[Symbol.asyncIterator]()
  const fast = scan.observations[Symbol.asyncIterator]()
  const advertise = value => h.native.emitAdvertisement(undefined, {
    localName: String(value), manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(512, value).toString('base64') }]
  })
  try {
    advertise(1)
    expect((await fast.next()).value.value.localName).toBe('1')
    advertise(2)
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    expect((await slow.next()).value).toMatchObject({ kind: 'overflow', policy: 'drop-newest', droppedItems: 1 })
    expect((await slow.next()).value.value.localName).toBe('1')
    expect((await fast.next()).value.value.localName).toBe('2')
  } finally {
    await slow.return()
    await fast.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('full and drained fanout queues replace one occurrence without losing presence ownership', async () => {
  let now = 1000
  let timeout
  const maximumAggregateRetainedBytes = 6000
  const h = await setup({ now: () => now, core: { maximumAggregateRetainedBytes } })
  h.core.scheduleDeadline = (_deadline, action) => {
    timeout = action
    return { cancel() {} }
  }
  const scan = await h.manager.scan({ delivery: 'latest', observation: { reportLostAfterMs: 10 } })
  const slow = scan.observations[Symbol.asyncIterator]()
  const fast = scan.observations[Symbol.asyncIterator]()
  const events = scan.lossAwareEventsV1[Symbol.asyncIterator]()
  const advertise = value => h.native.emitAdvertisement(undefined, {
    localName: String(value),
    manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, value).toString('base64') }]
  })
  const nextObservation = async reader => {
    let item = await reader.next()
    while (item.value.kind === 'overflow') item = await reader.next()
    return item.value.value
  }
  try {
    advertise(1)
    expect((await nextObservation(fast)).localName).toBe('1')
    advertise(2)
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    expect((await nextObservation(slow)).localName).toBe('2')
    expect((await nextObservation(fast)).localName).toBe('2')
    await expect(events.next()).resolves.toMatchObject({ value: { kind: 'stream-loss', subscriber: { droppedItems: 1 } } })
    expect((await events.next()).value.peer.lastAdvertisement.localName).toBe('2')
    await slow.return()
    await fast.return()
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThanOrEqual(1024)
    now += 10
    timeout()
    const lost = await events.next()
    expect(lost.value.kind).toBe('lost')
    expect(lost.value.peer.lastAdvertisement.manufacturerData[0].data[0]).toBe(2)
  } finally {
    await slow.return()
    await fast.return()
    await events.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('presence retains its advertisement under aggregate ownership after observation delivery', async () => {
  const h = await setup({ core: { maximumAggregateRetainedBytes: 8192 } })
  const scan = await h.manager.scan({ observation: { reportLostAfterMs: 60000 } })
  const reader = scan.observations[Symbol.asyncIterator]()
  const payload = Buffer.alloc(1024, 7)
  try {
    h.native.emitAdvertisement(undefined, {
      manufacturerData: [{ companyId: 107, payloadB64: payload.toString('base64') }]
    })
    const received = await reader.next()
    expect(received.value.value.manufacturerData[0].data).toHaveLength(payload.length)
    await settle(100)
    // Presence can later emit the peer's advertisement even after delivery.
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeGreaterThanOrEqual(payload.length)
  } finally {
    await reader.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('presence replacement progresses within aggregate quota', async () => {
  const maximumAggregateRetainedBytes = 6000
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ observation: { reportLostAfterMs: 60000 } })
  const reader = scan.observations[Symbol.asyncIterator]()
  const advertise = value => h.native.emitAdvertisement(undefined, {
    localName: String(value),
    manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, value).toString('base64') }]
  })
  try {
    advertise(1)
    expect((await reader.next()).value.kind).toBe('value')
    let delivered = false
    const second = reader.next().then(value => { delivered = true; return value })
    advertise(2)
    await settle(100)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    expect(delivered).toBe(true)
    expect((await second).value.value.localName).toBe('2')
  } finally {
    await reader.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('presence timeout releases both its occurrence and coalescing state', async () => {
  let now = 1000
  const h = await setup({ now: () => now, core: { maximumAggregateRetainedBytes: 8192 } })
  let timeout
  h.core.scheduleDeadline = (_deadline, action) => {
    timeout = action
    return { cancel() {} }
  }
  const scan = await h.manager.scan({ observation: { reportLostAfterMs: 10 } })
  const reader = scan.observations[Symbol.asyncIterator]()
  try {
    h.native.emitAdvertisement(undefined, {
      manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, 7).toString('base64') }]
    })
    expect((await reader.next()).value.kind).toBe('value')
    now += 10
    timeout()
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThan(128)
  } finally {
    await reader.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('split evidence releases replaced and expired payload charges without losing newer facts', async () => {
  let now = 1000
  const h = await setup({ now: () => now, core: { maximumAggregateRetainedBytes: 16384 } })
  const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{ names: { exact: ['Split target'] } }] } })
  const iterator = scan.observations[Symbol.asyncIterator]()
  const send = async (size, capture) => {
    const pending = iterator.next()
    h.native.emitAdvertisement(undefined, {
      localName: 'Split target', sourceTimestampMs: capture,
      manufacturerData: size === null ? null : [{ companyId: 107, payloadB64: Buffer.alloc(size, capture).toString('base64') }]
    })
    const result = await pending
    expect(result.value.kind).toBe('value')
    await settle(50)
    return result.value.value
  }
  try {
    expect((await send(2048, 1)).manufacturerData[0].data).toHaveLength(2048)
    const initial = h.core.core.aggregateQuota.retainedBytes()
    expect(initial).toBeGreaterThanOrEqual(2048)
    now++
    expect((await send(1024, 2)).manufacturerData[0].data).toHaveLength(1024)
    const replaced = h.core.core.aggregateQuota.retainedBytes()
    expect(replaced).toBeGreaterThanOrEqual(1024)
    expect(replaced).toBeLessThan(initial - 512)
    now += 10001
    // Null on the mobile wire means unreported, not an observed empty list.
    expect((await send(null, 3)).manufacturerData).toBeNull()
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThan(replaced - 512)
  } finally {
    await iterator.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('cache pressure keeps the public read pending and the source observation owned until cancellation', async () => {
  // Leave room for the next source claim so cache admission, rather than
  // source ingress, is the limiting owner after shared buffers are deduplicated.
  const maximumAggregateRetainedBytes = 9000
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scanInternal = h.core.scan.bind(h.core)
  let source
  h.core.scan = async options => {
    const session = await scanInternal(options)
    source = session.observations
    return session
  }
  const scan = await h.manager.scan({ duplicates: 'all', query: { anyOf: [{ names: { exact: ['Split target'] } }] } })
  const iterator = scan.observations[Symbol.asyncIterator]()
  let pending
  try {
    for (let i = 1; i <= 12; i++) {
      pending = iterator.next()
      let settled = false
      pending.then(() => { settled = true })
      h.native.emitAdvertisement(undefined, {
        localName: 'Split target', sourceTimestampMs: i,
        manufacturerData: [{ companyId: i, payloadB64: Buffer.alloc(1024, i).toString('base64') }]
      })
      await settle(100)
      expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
      if (!settled) break
      expect((await pending).value.kind).toBe('value')
      pending = undefined
    }
    expect(pending).toBeDefined()
    expect(source.retainedPayloadBytes()).toBeGreaterThanOrEqual(1024)
    await scan.observations.close()
    expect((await pending).value).toMatchObject({ kind: 'terminal', reason: 'closed', droppedItems: 1 })
  } finally {
    await iterator.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('a late public subscriber shares only later occurrences within the aggregate ceiling', async () => {
  const maximumAggregateRetainedBytes = 3500
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all', observation: { includeRawAdvertisement: true } })
  const first = scan.observations[Symbol.asyncIterator]()
  let second
  try {
    h.native.emitAdvertisement(undefined, { rssi: -80, rawRecordB64: 'AQIDBA==' })
    await settle(100)
    second = scan.observations[Symbol.asyncIterator]()
    for (let i = 0; i < 2; i++) {
      h.native.emitAdvertisement(undefined, { rssi: -79 + i, rawRecordB64: 'BQYHCA==' })
      await settle(30)
    }
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    const a = (await first.next()).value.value
    expect(a.rssi).toBe(-80)
    for (let i = 0; i < 2; i++) {
      const b = (await first.next()).value.value
      const c = (await second.next()).value.value
      expect([b.rssi, c.rssi]).toEqual([-79 + i, -79 + i])
      expect([...b.rawAdvertisement.value]).toEqual([5, 6, 7, 8])
      expect([...c.rawAdvertisement.value]).toEqual([5, 6, 7, 8])
      b.rawAdvertisement.value[0] = 99
      expect(c.rawAdvertisement.value[0]).toBe(5)
    }
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
  } finally {
    await first.return()
    if (second !== undefined) await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('cancelling an event subscriber releases only its shared occurrence reference', async () => {
  const h = await setup({ core: { maximumAggregateRetainedBytes: 4096 } })
  const scan = await h.manager.scan({ delivery: 'lossless-bounded', duplicates: 'all' })
  const observation = scan.observations[Symbol.asyncIterator]()
  const events = scan.events[Symbol.asyncIterator]()
  try {
    h.native.emitAdvertisement(undefined, {
      manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, 7).toString('base64') }]
    })
    await settle(100)
    const before = h.core.core.aggregateQuota.retainedBytes()
    await events.return()
    const after = h.core.core.aggregateQuota.retainedBytes()
    expect(after).toBeLessThan(before)
    expect(after).toBeGreaterThanOrEqual(1024)
    const item = await observation.next()
    expect(item.value.value.manufacturerData[0].data).toHaveLength(1024)
    expect(item.value.value.peer.lastAdvertisement.manufacturerData[0].data).toHaveLength(1024)
  } finally {
    await observation.return()
    await events.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
  expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
})

test('a source failure cannot retain an oversized terminal beyond the aggregate ceiling', async () => {
  const maximumAggregateRetainedBytes = 600
  const detail = 'failure '.repeat(150)
  const h = await setup({ scanEndV2: true, core: { maximumAggregateRetainedBytes, terminalEvidence: { maximumCanonicalBytes: 4096 } } })
  const scan = await h.manager.scan({ duplicates: 'all' })
  try {
    const session = h.native.liveSessions()[0]
    const membership = [...session.scans.keys()][0]
    const scope = h.native.opsInvoked('terminal-evidence.reserve')[0].scope
    const error = { code: 'adapter.powered-off', domain: 'adapter', operation: 'scan.start', retryability: 'never', detail, platform: null }
    const canonicalBytes = encodeTerminalEvidence({ kind: 'terminal', reason: 'source-failed', droppedItems: 0, droppedBytes: 0, replacedItems: 0, error }, scope)
    const digest = createHash('sha256').update(canonicalBytes).digest('hex')
    const reservationId = [...h.native.terminalEvidenceReservations.keys()][0]
    const committed = JSON.parse(await h.native.invoke(session.id, 'terminal-evidence.commit', JSON.stringify({ reservationId, canonicalBytesBase64: Buffer.from(canonicalBytes).toString('base64'), digest })))
    expect(committed.ok).toBe(true)
    session.scans.delete(membership)
    h.native.push(session, { t: 'scan-end', operationId: membership, startOperationId: h.native.opsInvoked('scan.start')[0].operationId, reason: 'source-failed', version: 2, source: 'adapter', generation: '1', error, evidence: { producer: 'scan', kind: committed.value.kind, ref: committed.value.ref, byteLength: committed.value.byteLength, digest: committed.value.digest, formatVersion: committed.value.formatVersion, durability: committed.value.durability, scope } })
    await settle(150)
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    const iterator = scan.observations[Symbol.asyncIterator]()
    const terminal = await iterator.next()
    expect(terminal.value).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed',
      error: {
        code: 'adapter.powered-off',
        domain: 'adapter',
        operation: 'scan.start',
        retryability: 'never',
        platform: null
      }
    })
    expect(h.native.opsInvoked('terminal-evidence.read')).toEqual([
      { ref: committed.value.ref, maximumBytes: 4096 },
      { ref: committed.value.ref, maximumBytes: 4096 }
    ])
    expect(h.core.core.aggregateQuota.retainedBytes()).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    await iterator.return()
  } finally {
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('public scan subscribers cannot mutate one another’s observation bytes', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  const first = scan.observations[Symbol.asyncIterator]()
  const second = scan.observations[Symbol.asyncIterator]()
  try {
    h.native.emitAdvertisement(undefined, { rawRecordB64: 'AQIDBA==' })
    const a = (await first.next()).value.value
    a.manufacturerData[0].data[0] = 42
    a.peer.lastAdvertisement.manufacturerData[0].data[0] = 43
    a.rawAdvertisement.value[0] = 44
    const b = (await second.next()).value.value
    expect([...b.manufacturerData[0].data]).toEqual([0, 128, 255])
    expect([...b.peer.lastAdvertisement.manufacturerData[0].data]).toEqual([0, 128, 255])
    expect([...b.rawAdvertisement.value]).toEqual([1, 2, 3, 4])
  } finally {
    await first.return()
    await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('public scan event subscribers cannot mutate one another’s peer bytes', async () => {
  const h = await setup()
  const scan = await h.manager.scan({ duplicates: 'all' })
  const first = scan.events[Symbol.asyncIterator]()
  const second = scan.events[Symbol.asyncIterator]()
  try {
    h.native.emitAdvertisement()
    const a = (await first.next()).value
    a.peer.lastAdvertisement.manufacturerData[0].data[0] = 42
    const b = (await second.next()).value
    expect([...b.peer.lastAdvertisement.manufacturerData[0].data]).toEqual([0, 128, 255])
  } finally {
    await first.return()
    await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
  }
})

test('one matched source occurrence atomically reserves evidence and two observation subscribers', async () => {
  const maximumAggregateRetainedBytes = 4096
  const h = await setup({ core: { maximumAggregateRetainedBytes } })
  const scan = await h.manager.scan({
    duplicates: 'all',
    delivery: 'lossless-bounded',
    observation: { includeRawAdvertisement: true },
    query: { anyOf: [{ names: { exact: ['Split target'] } }] }
  })
  const first = scan.observations[Symbol.asyncIterator]()
  const second = scan.observations[Symbol.asyncIterator]()
  try {
    h.native.emitAdvertisement(undefined, {
      localName: 'Split target', sourceTimestampMs: 1, rawRecordB64: 'AQIDBA==',
      manufacturerData: [{ companyId: 107, payloadB64: Buffer.alloc(1024, 7).toString('base64') }]
    })
    await settle(100)
    const retainedWithBoth = h.core.core.aggregateQuota.retainedBytes()
    expect(retainedWithBoth).toBeLessThanOrEqual(maximumAggregateRetainedBytes)
    const a = (await first.next()).value.value
    expect(a.manufacturerData[0].data).toHaveLength(1024)
    expect([...a.rawAdvertisement.value]).toEqual([1, 2, 3, 4])
    const retainedWithSecond = h.core.core.aggregateQuota.retainedBytes()
    expect(retainedWithSecond).toBeLessThan(retainedWithBoth)
    a.manufacturerData[0].data[0] = 99
    a.rawAdvertisement.value[0] = 99
    const b = (await second.next()).value.value
    expect(b.manufacturerData[0].data[0]).toBe(7)
    expect([...b.rawAdvertisement.value]).toEqual([1, 2, 3, 4])
    const retainedEvidence = h.core.core.aggregateQuota.retainedBytes()
    expect(retainedEvidence).toBeLessThan(retainedWithSecond)
    expect(retainedEvidence).toBeGreaterThanOrEqual(1024)
  } finally {
    await first.return()
    await second.return()
    await scan.observations.close()
    await h.manager.destroy()
    await h.internal.destroy()
    expect(h.core.core.aggregateQuota.retainedBytes()).toBe(0)
  }
})
