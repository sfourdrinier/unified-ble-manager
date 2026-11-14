'use strict'

const { advertisementMatchesFilter } = require('../../src/backend-contract/advertisement')
const { canonicalUuid, capacity, monotonicTimestamp, opaqueId } = require('../../src/backend-contract/primitives')
const {
  ScanEvidenceSession,
  SCAN_EVIDENCE_WINDOW_MS,
  SCAN_EVIDENCE_PEER_CAPACITY
} = require('../../src/backend-contract/scan-evidence')
const { filterScanObservations } = require('../../src/public/ble-manager')
const { normalizeScanQuery, normalizeScanObservation } = require('../../src/public/scan-query')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')
const { advertisementByteLength } = require('../../src/core/unified-ble-core-helpers')

const HEART = '0000180d-0000-1000-8000-00805f9b34fb'

function absent() {
  return Object.freeze({ state: 'absent', reason: 'not in this packet', provenance: 'not-provided' })
}

function present(value) {
  return Object.freeze({ state: 'present', value, provenance: 'observed' })
}

function packet({
  address,
  addressType = 'public',
  name = null,
  services = null,
  at = 0,
  session = 'scan-1',
  backend = 'backend',
  capture = null,
  clockScope = 'native-clock',
  origin = 'platform',
  ordinal = 1,
  manufacturer = null,
  data = null,
  connectable = null,
  rssi = null
}) {
  return Object.freeze({
    device: Object.freeze({
      id: opaqueId(`peer-${address}`, 'peer', 'test'),
      backendInstanceId: opaqueId(backend, 'backend', 'test'),
      scope: 'backend',
      stableAcrossRestarts: false,
      address: Object.freeze({ value: address, type: addressType })
    }),
    provenance: 'platform-raw',
    origin: 'advertisement',
    sourceTimestamp: capture === null ? absent() : present({ monotonicMs: capture, origin, ...(clockScope === null ? {} : { clockScope }) }),
    receivedAtMonotonicMs: monotonicTimestamp(at),
    ingressOrdinal: ordinal,
    scanSessionId: opaqueId(session, 'scan-session', 'test'),
    localName: name === null ? absent() : present(name),
    rssi: rssi === null ? absent() : present(rssi),
    txPower: absent(),
    connectable: connectable === null ? absent() : present(connectable),
    appearance: absent(),
    serviceUuids: services === null ? present(Object.freeze([])) : present(Object.freeze(services)),
    solicitedServiceUuids: absent(),
    overflowServiceUuids: absent(),
    serviceData: present(Object.freeze(data ?? [])),
    manufacturerData: present(Object.freeze(manufacturer ?? [])),
    rawRecord: absent(),
    scanResponseRecord: absent()
  })
}

const filter = Object.freeze({
  serviceUuids: Object.freeze([canonicalUuid(HEART)]),
  manufacturerData: Object.freeze([]),
  localNamePrefix: 'Polar H10'
})

function keep(session, observation) {
  return session.matchAdvertisement(observation, candidate => advertisementMatchesFilter(filter, candidate))
}

describe('split advertisement evidence', () => {
  test('core scan accounting includes variable observation metadata', () => {
    const short = packet({ address: 'peer', name: 'x', capture: 1, clockScope: 'a' })
    const long = packet({ address: 'peer', name: 'x'.repeat(2049), capture: 1, clockScope: 'a'.repeat(1025) })
    expect(advertisementByteLength(long) - advertisementByteLength(short)).toBeGreaterThanOrEqual(2 * (2048 + 1024))
  })
  test('core charges peer identity and each UUID array entry', () => {
    const base = packet({ address: 'p', services: [] })
    const larger = packet({ address: 'p'.repeat(100), services: [canonicalUuid(HEART)] })
    expect(advertisementByteLength(larger) - advertisementByteLength(base)).toBeGreaterThanOrEqual(2 * (99 + 36) + 8)
  })
  test('core charges a shared backing buffer once and releases the retained charge on delivery', async () => {
    const backing = new Uint8Array(4096)
    const observation = packet({ address: 'shared', manufacturer: [
      { companyIdentifier: 1, value: backing.subarray(0, 1) },
      { companyIdentifier: 2, value: backing.subarray(1, 2) }
    ] })
    const small = packet({ address: 'shared', manufacturer: [
      { companyIdentifier: 1, value: new Uint8Array(1) },
      { companyIdentifier: 2, value: new Uint8Array(1) }
    ] })
    expect(advertisementByteLength(observation) - advertisementByteLength(small)).toBe(4094)
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 128, reservedControlCapacity: 0 }, 'error', true)
    const quota = new AggregateStreamQuota(10000)
    quota.register(stream)
    expect(quota.emit(stream, observation, 66, null, 2, advertisementByteLength(observation)).accepted).toBe(true)
    expect(quota.retainedBytes()).toBeGreaterThanOrEqual(4096)
    expect((await stream[Symbol.asyncIterator]().next()).value.kind).toBe('value')
    expect(quota.retainedBytes()).toBe(0)
  })

  test('logical delivery payload can exceed its shared retained backing buffer', async () => {
    const backing = new Uint8Array(1024)
    const observation = packet({ address: 'overlap', manufacturer: [
      { companyIdentifier: 1, value: backing }, { companyIdentifier: 2, value: backing }
    ] })
    const retained = advertisementByteLength(observation)
    expect(retained).toBeLessThan(2048)
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 4096, reservedControlCapacity: 0 }, 'error', true)
    const quota = new AggregateStreamQuota(4096)
    quota.register(stream)
    expect(quota.emit(stream, observation, 2112, null, 2048, retained).accepted).toBe(true)
    expect((await stream[Symbol.asyncIterator]().next()).value.kind).toBe('value')
    expect(quota.retainedBytes()).toBe(0)
  })

  test('drop-oldest releases the old retained charge independently of delivery bytes', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 128, reservedControlCapacity: 0 }, 'drop-oldest', true)
    const quota = new AggregateStreamQuota(5000)
    quota.register(stream)
    const first = packet({ address: 'a', name: 'x'.repeat(1000) })
    const second = packet({ address: 'b', name: 'new' })
    expect(quota.emit(stream, first, 64, null, 0, advertisementByteLength(first)).accepted).toBe(true)
    expect(quota.emit(stream, second, 64, null, 0, advertisementByteLength(second)).accepted).toBe(true)
    expect(quota.retainedBytes()).toBe(advertisementByteLength(second))
    const iterator = stream[Symbol.asyncIterator]()
    expect((await iterator.next()).value.kind).toBe('overflow')
    expect((await iterator.next()).value.value.localName.value).toBe('new')
    expect(quota.retainedBytes()).toBe(0)
  })

  test('evidence charges a shared clock scope once across facts and releases it on clear', () => {
    let charged = 0
    const session = new ScanEvidenceSession(bytes => { charged = bytes; return true })
    const measure = scope => {
      session.clear()
      session.matchAdvertisement(packet({ address: 'clock', capture: 1, clockScope: scope,
        name: 'name', services: [canonicalUuid(HEART)], rssi: -50 }), () => false)
      return charged
    }
    expect(measure('x'.repeat(1025)) - measure('x')).toBe(2048)
    session.clear()
    expect(charged).toBe(0)
  })
  test('a staged candidate does not change live facts until committed', () => {
    const session = new ScanEvidenceSession(() => true)
    const first = packet({ address: 'staged', name: 'Polar H10', at: 1 })
    const second = packet({ address: 'staged', services: [canonicalUuid(HEART)], at: 2, ordinal: 2 })
    const staged = session.stage(candidate => keep(candidate, first))
    expect(staged.matched).toBeNull()
    expect(staged.bytes).toBeGreaterThan(0)
    expect(keep(session, second)).toBeNull()
    const replacement = session.stage(candidate => keep(candidate, first))
    replacement.commit()
    expect(keep(session, second)).not.toBeNull()
  })

  test('a refused replacement leaves the accepted fact and its ordering watermark intact', () => {
    let limit = 100000
    let charged = 0
    const session = new ScanEvidenceSession(bytes => {
      if (bytes > limit) return false
      charged = bytes
      return true
    })
    const initial = packet({ address: 'admission', capture: 200, at: 1,
      manufacturer: [{ companyIdentifier: 7, value: new Uint8Array(32).fill(2) }] })
    session.matchAdvertisement(initial, () => false)
    const before = charged
    expect(before).toBeGreaterThanOrEqual(32)
    limit = before
    expect(() => session.matchAdvertisement(packet({ address: 'admission', capture: 300, at: 2,
      manufacturer: [{ companyIdentifier: 7, value: new Uint8Array(2048) }] }), () => false)).toThrow()
    expect(charged).toBe(before)
    const selected = session.matchAdvertisement(packet({ address: 'admission', capture: 250, at: 3 }), candidate =>
      candidate.manufacturerData.value?.[0]?.value[0] === 2)
    expect(selected.manufacturerData.value[0].value).toHaveLength(32)
    session.clear()
    expect(charged).toBe(0)
  })

  test('expiry releases the backing buffer while retaining a charged source watermark', () => {
    let charged = 0
    const session = new ScanEvidenceSession(bytes => { charged = bytes; return true })
    const backing = new Uint8Array(4096)
    session.matchAdvertisement(packet({ address: 'backing', capture: 200, at: 0,
      manufacturer: [{ companyIdentifier: 7, value: backing.subarray(0, 1) }] }), () => false)
    const before = charged
    expect(before).toBeGreaterThanOrEqual(backing.byteLength)
    session.matchAdvertisement(packet({ address: 'backing', capture: 300, at: 10001 }), () => false)
    expect(charged).toBeGreaterThan(0)
    expect(charged).toBeLessThan(before - 4000)
    const watermarkCharge = charged
    session.matchAdvertisement(packet({ address: 'backing', capture: 100, at: 10002,
      manufacturer: [{ companyIdentifier: 7, value: new Uint8Array(64) }] }), () => false)
    expect(charged).toBe(watermarkCharge)
    session.clear()
    expect(charged).toBe(0)
  })

  test('the filtered stream charges its cache and clears it when closed', async () => {
    const source = new CoreBoundedStream({ itemCapacity: 4, byteCapacity: 8192, reservedControlCapacity: 0 }, 'error', true)
    const quota = new AggregateStreamQuota(8192)
    quota.register(source)
    const query = normalizeScanQuery({ anyOf: [{ names: { exact: ['target'] } }] })
    const filtered = filterScanObservations(source, query)
    const iterator = filtered[Symbol.asyncIterator]()
    source.emit(packet({ address: 'filtered', name: 'target', manufacturer: [{ companyIdentifier: 7, value: new Uint8Array(1024) }] }), 1200)
    expect((await iterator.next()).value.kind).toBe('value')
    expect(quota.retainedBytes()).toBeGreaterThanOrEqual(1024)
    await filtered.close()
    expect(quota.retainedBytes()).toBe(0)
  })
  test('charges a shared backing buffer once across retained packet fields', () => {
    let charged = 0
    const session = new ScanEvidenceSession(bytes => { charged = bytes; return true })
    const backing = new Uint8Array(4096)
    const make = (size) => packet({ address: 'shared', manufacturer: [
      { companyIdentifier: 7, value: backing.subarray(0, size) },
      { companyIdentifier: 8, value: backing.subarray(1, size + 1) }
    ] })
    session.matchAdvertisement(make(1), () => false)
    const sharedCharge = charged
    const other = new ScanEvidenceSession(bytes => { charged = bytes; return true })
    other.matchAdvertisement(packet({ address: 'shared', manufacturer: [
      { companyIdentifier: 7, value: new Uint8Array(1) },
      { companyIdentifier: 8, value: new Uint8Array(1) }
    ] }), () => false)
    expect(sharedCharge - charged).toBe(4094)
  })

  test('filtered delivery waits for cache capacity and retries the same claimed observation', async () => {
    const limits = { itemCapacity: 4, byteCapacity: 8192, reservedControlCapacity: 0 }
    const source = new CoreBoundedStream(limits, 'error', true)
    const blocker = new CoreBoundedStream(limits, 'error', true)
    const quota = new AggregateStreamQuota(3000)
    quota.register(source)
    quota.register(blocker)
    quota.emit(blocker, 'blocker', 1800)
    quota.emit(source, packet({ address: 'retry', name: 'target', manufacturer: [
      { companyIdentifier: 7, value: new Uint8Array(1024) }
    ] }), 1100)
    const filtered = filterScanObservations(source, normalizeScanQuery({ anyOf: [{ names: { exact: ['target'] } }] }))
    const iterator = filtered[Symbol.asyncIterator]()
    const pending = iterator.next()
    let settled = false
    pending.then(() => { settled = true }, () => { settled = true })
    await new Promise(resolve => setTimeout(resolve, 30))
    expect(settled).toBe(false)
    await blocker.close()
    expect((await pending).value.value.manufacturerData[0].data).toHaveLength(1024)
    await filtered.close()
    expect(quota.retainedBytes()).toBe(0)
  })

  test('an impossible evidence admission reports quota and leaves the source occurrence retrievable', async () => {
    const limits = { itemCapacity: 4, byteCapacity: 8192, reservedControlCapacity: 0 }
    const source = new CoreBoundedStream(limits, 'error', true)
    const quota = new AggregateStreamQuota(1500)
    quota.register(source)
    const name = 'x'.repeat(2048)
    quota.emit(source, packet({ address: 'impossible', name }), 100)
    const filtered = filterScanObservations(source, normalizeScanQuery({ anyOf: [{ names: { exact: [name] } }] }))
    const iterator = filtered[Symbol.asyncIterator]()
    const pending = iterator.next().then(() => 'delivered', error => error.normalized?.code ?? error.code)
    const outcome = await pending
    expect(outcome).toBe('stream.quota')
    expect((await source[Symbol.asyncIterator]().next()).value.value.localName.value).toBe(name)
    await filtered.close()
    await source.close()
  })

  test('cancelling a cache-blocked filtered read releases the claim and wakes the read', async () => {
    const limits = { itemCapacity: 4, byteCapacity: 8192, reservedControlCapacity: 0 }
    const source = new CoreBoundedStream(limits, 'error', true)
    const quota = new AggregateStreamQuota(1500)
    quota.register(source)
    quota.emit(source, packet({ address: 'cancel', name: 'target', manufacturer: [
      { companyIdentifier: 7, value: new Uint8Array(1024) }
    ] }), 1100)
    const filtered = filterScanObservations(source, normalizeScanQuery({ anyOf: [{ names: { exact: ['target'] } }] }))
    const iterator = filtered[Symbol.asyncIterator]()
    const pending = iterator.next()
    await new Promise(resolve => setTimeout(resolve, 30))
    await iterator.return()
    await expect(pending).resolves.toMatchObject({ done: true })
    expect((await source[Symbol.asyncIterator]().next()).value.kind).toBe('value')
    await filtered.close()
  })

  test('cache retries do not refresh compact packet receipt time', async () => {
    let now = 0
    const limits = { itemCapacity: 4, byteCapacity: 8192, reservedControlCapacity: 0 }
    const source = new CoreBoundedStream(limits, 'error', true)
    const blocker = new CoreBoundedStream(limits, 'error', true)
    const quota = new AggregateStreamQuota(1400)
    quota.register(source)
    quota.register(blocker)
    quota.emit(blocker, 'blocker', 1200)
    const compact = (peerId, localName, serviceUuids) => ({
      peerId, localName, serviceUuids, rssi: null, manufacturerData: [], serviceData: []
    })
    quota.emit(source, compact('peer', 'target', []), 100)
    const query = normalizeScanQuery({ anyOf: [{ names: { exact: ['target'] }, services: { any: [HEART] } }] })
    const filtered = filterScanObservations(source, query, 'all', () => now)
    const pending = filtered[Symbol.asyncIterator]().next()
    await new Promise(resolve => setTimeout(resolve, 30))
    now = 10001
    await blocker.close()
    await new Promise(resolve => setTimeout(resolve, 40))
    quota.emit(source, compact('peer', null, [HEART]), 100)
    now = 10002
    quota.emit(source, compact('complete', 'target', [HEART]), 100)
    expect((await pending).value.value.peer.id).toBe('complete')
    await filtered.close()
  })

  test('an older captured packet cannot borrow name evidence from its captured future', () => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'order', name: 'Polar H10 new', capture: 200, at: 200 }))
    expect(keep(session, packet({ address: 'order', services: [canonicalUuid(HEART)], capture: 100, at: 300 }))).toBeNull()
    const forward = keep(session, packet({ address: 'order', services: [canonicalUuid(HEART)], capture: 300, at: 400 }))
    expect(forward.localName.value).toBe('Polar H10 new')
  })

  test.each(['name', 'connectable', 'rssi', 'manufacturer', 'data'])('older same-key %s does not replace or refresh accepted source evidence', field => {
    const session = new ScanEvidenceSession()
    const changes = (value) => ({
      name: { name: value ? 'accepted' : 'stale' }, connectable: { connectable: value }, rssi: { rssi: value ? -40 : -90 },
      manufacturer: { manufacturer: [{ companyIdentifier: 7, value: new Uint8Array([value ? 2 : 1]) }] },
      data: { data: [{ serviceUuid: canonicalUuid(HEART), value: new Uint8Array([value ? 2 : 1]) }] }
    })[field]
    session.matchAdvertisement(packet({ address: 'same-key', at: 100, capture: 200, ...changes(true) }), () => false)
    session.matchAdvertisement(packet({ address: 'same-key', at: 200, capture: 100, ...changes(false) }), () => false)
    const selected = candidate => ({ name: candidate.localName.value === 'accepted', connectable: candidate.connectable.value === true,
      rssi: candidate.rssi.value === -40, manufacturer: candidate.manufacturerData.value?.[0]?.value[0] === 2,
      data: candidate.serviceData.value?.[0]?.value[0] === 2 })[field]
    const late = packet({ address: 'same-key', at: 300, capture: 300 })
    expect(session.matchAdvertisement(late, selected)).not.toBeNull()
    session.matchAdvertisement(packet({ address: 'same-key', at: 10_200, capture: 100, ...changes(false) }), () => false)
    expect(session.matchAdvertisement(packet({ address: 'same-key', at: 10_201, capture: 400 }), selected)).toBeNull()
  })

  test('expired payload storage is released while bounded source watermarks reject repeated captures', () => {
    const session = new ScanEvidenceSession()
    const first = packet({ address: 'watermark', capture: 200, at: 0,
      manufacturer: [{ companyIdentifier: 7, value: new Uint8Array([2]) }] })
    session.matchAdvertisement(first, () => false)
    session.matchAdvertisement(packet({ address: 'watermark', capture: 300, at: 10001 }), () => false)
    const stored = [...session.advertisements.values()][0].facts.get('manufacturer:7')
    expect(stored.manufacturerData).toBeNull()
    expect(stored.sourceTime.monotonicMs).toBe(200)
    session.matchAdvertisement({ ...first, receivedAtMonotonicMs: monotonicTimestamp(11000) }, () => false)
    expect(session.matchAdvertisement(packet({ address: 'watermark', capture: 400, at: 11001 }), candidate =>
      candidate.manufacturerData.state === 'present' && candidate.manufacturerData.value.length > 0)).toBeNull()
  })

  test('identical source timestamp and ordinal cannot renew TTL at expiry', () => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'identical', name: 'Polar H10 duplicate', capture: 200, at: 0, ordinal: 1 }))
    keep(session, packet({ address: 'identical', name: 'Polar H10 duplicate', capture: 200, at: 10001, ordinal: 1 }))
    expect(keep(session, packet({ address: 'identical', services: [canonicalUuid(HEART)], capture: 300, at: 10002 }))).toBeNull()
  })

  test('a duplicate source capture with a newer ordinal cannot renew receipt freshness', () => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'duplicate', name: 'Polar H10 first', capture: 200, at: 0, ordinal: 1 }))
    keep(session, packet({ address: 'duplicate', name: 'Polar H10 second', capture: 200, at: 9000, ordinal: 2 }))
    expect(keep(session, packet({ address: 'duplicate', services: [canonicalUuid(HEART)], capture: 300, at: 10001 }))).toBeNull()
  })

  test('equal captures choose larger ingress ordinal regardless of delivery order', () => {
    const session = new ScanEvidenceSession()
    session.matchAdvertisement(packet({ address: 'equal', name: 'winner', capture: 200, at: 100, ordinal: 2 }), () => false)
    session.matchAdvertisement(packet({ address: 'equal', name: 'older ordinal', capture: 200, at: 200, ordinal: 1 }), () => false)
    const merged = session.matchAdvertisement(packet({ address: 'equal', capture: 300, at: 300 }), candidate => candidate.localName.value === 'winner')
    expect(merged.localName.value).toBe('winner')
  })

  test.each([{ clockScope: 'other-clock' }, { clockScope: null }, { origin: 'backend' }, { capture: null }])('incomparable capture metadata uses receipt ordering: %j', timing => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'incomparable', name: 'Polar H10 name', capture: 200, at: 100 }))
    expect(keep(session, packet({ address: 'incomparable', services: [canonicalUuid(HEART)], capture: 1, at: 200, ...timing }))).not.toBeNull()
  })

  test.each(['manufacturerData', 'serviceData', 'serviceUuids'])('source-rejected %s becomes absent rather than an invented empty list', field => {
    const session = new ScanEvidenceSession()
    const payloads = { manufacturer: [{ companyIdentifier: 7, value: new Uint8Array([2]) }],
      data: [{ serviceUuid: canonicalUuid(HEART), value: new Uint8Array([2]) }], services: [canonicalUuid(HEART)] }
    session.matchAdvertisement(packet({ address: 'list-absence', capture: 200, at: 100, ...payloads }), () => false)
    const older = packet({ address: 'list-absence', capture: 100, at: 200, name: 'packet name', ...payloads })
    const merged = session.matchAdvertisement(older, candidate => candidate.provenance === 'core-merged')
    expect(merged[field]).toMatchObject({ state: 'absent', provenance: 'derived' })
    expect(older[field]).toMatchObject({ state: 'present' })
  })

  test('a genuine raw observed empty list stays observed empty in a merged projection', () => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'observed-empty', services: [canonicalUuid(HEART)], capture: 100, at: 100 }))
    const name = packet({ address: 'observed-empty', name: 'Polar H10', capture: 200, at: 200 })
    const merged = keep(session, name)
    expect(merged.manufacturerData).toBe(name.manufacturerData)
    expect(merged.manufacturerData).toEqual({ state: 'present', provenance: 'observed', value: [] })
  })

  test('a complete older raw packet passes unchanged and does not replace newer cached name', () => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'raw', name: 'Polar H10 newer', capture: 200, at: 100 }))
    const old = packet({ address: 'raw', name: 'Polar H10 older', services: [canonicalUuid(HEART)], capture: 100, at: 200 })
    expect(keep(session, old)).toBe(old)
    expect(keep(session, packet({ address: 'raw', services: [canonicalUuid(HEART)], capture: 300, at: 300 })).localName.value).toBe('Polar H10 newer')
  })


  test('downstream filtering does not turn an upstream merged projection into fresh radio facts', () => {
    const upstream = new ScanEvidenceSession()
    const downstream = new ScanEvidenceSession()
    keep(upstream, packet({ address: 'projection-peer', name: 'Polar H10 0001', at: 0 }))
    const merged = keep(upstream, packet({ address: 'projection-peer', services: [canonicalUuid(HEART)], at: 9_000 }))
    expect(keep(downstream, merged)).not.toBeNull()
    expect(
      keep(downstream, packet({ address: 'projection-peer', services: [canonicalUuid(HEART)], at: 15_000 }))
    ).toBeNull()
  })

  test('IPC merged projections do not renew facts in a later filtering cache', () => {
    const session = new ScanEvidenceSession()
    const base = {
      peerId: 'projection-peer',
      address: 'A0:00:00:00:00:01',
      addressType: 'public',
      localName: 'Polar H10 0001',
      serviceUuids: [HEART],
      manufacturerData: [],
      serviceData: [],
      rssi: -50
    }
    const matches = candidate => candidate.localName?.startsWith('Polar H10') && candidate.serviceUuids.includes(HEART)
    expect(session.matchIpc({ ...base, provenance: 'core-merged' }, 9_000, 'scan', matches)).not.toBeNull()
    expect(session.matchIpc({ ...base, localName: null }, 15_000, 'scan', matches)).toBeNull()
  })

  test('an unexpired peer capacity refusal is explicit and does not replace retained peers', () => {
    const session = new ScanEvidenceSession()
    for (let index = 0; index < SCAN_EVIDENCE_PEER_CAPACITY; index += 1) {
      keep(session, packet({ address: `live-${index}`, name: 'Polar H10 0001', at: 0 }))
    }
    expect(() => keep(session, packet({ address: 'overflow', at: 1 }))).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'stream.quota' }) })
    )
    expect(session.advertisements.size).toBe(SCAN_EVIDENCE_PEER_CAPACITY)
    expect(keep(session, packet({ address: 'live-0', services: [canonicalUuid(HEART)], at: 1 }))).not.toBeNull()
    session.clear()
    expect(session.advertisements.size).toBe(0)
  })

  test('in-window service lists union even when the newest packet carries a nonempty list', () => {
    const session = new ScanEvidenceSession()
    const battery = canonicalUuid('0000180f-0000-1000-8000-00805f9b34fb')
    const matches = observation =>
      observation.serviceUuids.state === 'present' &&
      [canonicalUuid(HEART), battery].every(service => observation.serviceUuids.value.includes(service))
    expect(
      session.matchAdvertisement(packet({ address: 'same', services: [canonicalUuid(HEART)], at: 0 }), matches)
    ).toBeNull()
    const matched = session.matchAdvertisement(packet({ address: 'same', services: [battery], at: 1 }), matches)
    expect(matched.serviceUuids.value).toEqual([canonicalUuid(HEART), battery])
    expect(matched.serviceUuids.provenance).toBe('derived')
  })

  test('unrelated packets do not renew an old name or service fact', () => {
    const session = new ScanEvidenceSession()
    keep(session, packet({ address: 'same', name: 'Polar H10 0001', at: 0 }))
    for (const at of [5_000, 10_000, 15_000, 20_000]) {
      const matched = keep(session, packet({ address: 'same', services: [canonicalUuid(HEART)], at }))
      expect(matched !== null).toBe(at <= SCAN_EVIDENCE_WINDOW_MS)
    }
  })

  test.each([{ addressType: 'random' }, { backend: 'other-backend' }, { session: 'other-scan' }])(
    'peer scope prevents evidence borrowing: %j',
    identity => {
      const session = new ScanEvidenceSession()
      keep(session, packet({ address: 'same', name: 'Polar H10 0001', at: 0 }))
      expect(
        keep(session, packet({ address: 'same', services: [canonicalUuid(HEART)], at: 1, ...identity }))
      ).toBeNull()
    }
  )

  test('receipt progress expires inactive peers globally', () => {
    const session = new ScanEvidenceSession()
    for (let index = 0; index < 2_000; index += 1) {
      keep(session, packet({ address: `peer-${index}`, name: 'Polar H10 0001', at: 0 }))
    }
    keep(session, packet({ address: 'new', at: 60_000 }))
    expect(session.advertisements.size).toBe(1)
    expect(keep(session, packet({ address: 'peer-0', services: [canonicalUuid(HEART)], at: 60_001 }))).toBeNull()
  })

  test('a minimal compact packet with no carried additions produces no spurious merged projection', () => {
    const session = new ScanEvidenceSession()
    const incoming = { peerId: 'minimal', localName: 'not matching', rssi: -40,
      serviceUuids: [], manufacturerData: [], serviceData: [] }
    const seen = []
    expect(session.matchIpc(incoming, 100, 'scan', candidate => { seen.push(candidate); return false })).toBeNull()
    expect(seen).toEqual([incoming])
  })

  test('a legitimately merged minimal compact packet preserves optional field absence and raw empty lists', () => {
    const session = new ScanEvidenceSession()
    const base = { peerId: 'minimal', localName: null, rssi: null, serviceUuids: [], manufacturerData: [], serviceData: [] }
    const match = candidate => candidate.localName === 'target' && candidate.serviceUuids.includes(HEART)
    expect(session.matchIpc({ ...base, serviceUuids: [HEART] }, 100, 'scan', match)).toBeNull()
    const incoming = { ...base, localName: 'target' }
    const merged = session.matchIpc(incoming, 200, 'scan', match)
    expect(merged).toMatchObject({ provenance: 'core-merged', localName: 'target', serviceUuids: [HEART], rssi: null })
    expect(merged).not.toHaveProperty('connectable')
    expect(merged).not.toHaveProperty('txPowerLevel')
    expect(merged.manufacturerData).toBe(incoming.manufacturerData)
    expect(merged.serviceData).toBe(incoming.serviceData)
  })

  test.each([false, true])('actual public filter accepts minimal/mixed compact conjunction: previous full=%s', async full => {
    const source = new CoreBoundedStream({ itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) }, 'drop-oldest')
    const base = { peerId: 'minimal-public', localName: null, rssi: null, serviceUuids: [], manufacturerData: [], serviceData: [] }
    const query = normalizeScanQuery({ anyOf: [{ names: { exact: ['target'] }, services: { all: [HEART] }, ...(full ? { connectable: true } : {}) }] })
    const filtered = filterScanObservations(source, query, 'all', () => 100)
    const iterator = filtered[Symbol.asyncIterator]()
    const pending = iterator.next()
    source.emit({ ...base, localName: 'target', ...(full ? { connectable: true, txPowerLevel: -5 } : {}) }, 32)
    source.emit({ ...base, serviceUuids: [HEART] }, 32)
    source.finishWithReason('closed')
    const item = await pending
    expect(item).toMatchObject({ value: { kind: 'value', value: { localName: 'target', serviceUuids: [HEART], provenance: 'core-merged' } } })
    expect(item.value.value.connectable).toBe(full ? true : null)
    await iterator.return()
  })

  test('mixed full/minimal compact merge truthfully marks current unreported TX power unknown', () => {
    const session = new ScanEvidenceSession()
    const base = { peerId: 'mixed', localName: null, rssi: null, serviceUuids: [], manufacturerData: [], serviceData: [] }
    const match = candidate => candidate.localName === 'target' && candidate.serviceUuids.includes(HEART) && candidate.connectable === true
    session.matchIpc({ ...base, localName: 'target', connectable: true, txPowerLevel: -5 }, 100, 'scan', match)
    const merged = session.matchIpc({ ...base, serviceUuids: [HEART] }, 200, 'scan', match)
    expect(merged).toMatchObject({ connectable: true, txPowerLevel: null })
    expect(() => normalizeScanObservation(merged)).not.toThrow()
  })

  test('compact IPC uses delivery order when receipt precision ties and native ordinal is unavailable', () => {
    const session = new ScanEvidenceSession()
    const base = { peerId: 'receipt-tie', localName: null, rssi: null, txPowerLevel: null, serviceUuids: [], manufacturerData: [], serviceData: [] }
    session.matchIpc({ ...base, localName: 'old' }, 100, 'scan', () => false)
    session.matchIpc({ ...base, localName: 'new' }, 100, 'scan', () => false)
    expect(session.matchIpc({ ...base, serviceUuids: [HEART] }, 101, 'scan', candidate =>
      candidate.localName === 'new' && candidate.serviceUuids.includes(HEART)).localName).toBe('new')
  })

  test('IPC list evidence unions by key and each key expires independently', () => {
    const session = new ScanEvidenceSession()
    const base = {
      peerId: 'peer',
      address: 'same',
      addressType: 'public',
      localName: null,
      rssi: null,
      txPowerLevel: null,
      serviceUuids: [],
      manufacturerData: [],
      serviceData: []
    }
    const matches = candidate =>
      candidate.manufacturerData.some(entry => entry.companyId === 1) &&
      candidate.manufacturerData.some(entry => entry.companyId === 2)
    const first = { ...base, manufacturerData: [{ companyId: 1, data: new Uint8Array([1]) }] }
    const second = { ...base, manufacturerData: [{ companyId: 2, data: new Uint8Array([2]) }] }
    expect(session.matchIpc(first, 0, 'scan', matches)).toBeNull()
    expect(session.matchIpc(second, 1, 'scan', matches).manufacturerData).toHaveLength(2)
    expect(session.matchIpc(second, SCAN_EVIDENCE_WINDOW_MS + 1, 'scan', matches)).toBeNull()
  })

  test('a service packet and a later name packet match together, and the raw packet stays raw', () => {
    const session = new ScanEvidenceSession()
    const services = packet({
      address: 'A0:9E:1A:E9:B9:3D',
      services: [canonicalUuid(HEART)],
      at: 1_000
    })
    const name = packet({ address: 'A0:9E:1A:E9:B9:3D', name: 'Polar H10 0001', at: 1_200 })
    expect(keep(session, services)).toBeNull()
    expect(services.provenance).toBe('platform-raw')
    expect(services.origin).toBe('advertisement')
    const matched = keep(session, name)
    expect(matched).not.toBeNull()
    expect(matched.provenance).toBe('core-merged')
    expect(matched.origin).toBeUndefined()
    expect(matched.localName.value).toBe('Polar H10 0001')
    expect(matched.serviceUuids.value).toEqual([canonicalUuid(HEART)])
    expect(name.serviceUuids.value).toEqual([])
  })

  test('the name can arrive before the services', () => {
    const session = new ScanEvidenceSession()
    const name = packet({ address: 'A0:9E:1A:E9:B9:3D', name: 'Polar H10 0001', at: 1_000 })
    const services = packet({
      address: 'A0:9E:1A:E9:B9:3D',
      services: [canonicalUuid(HEART)],
      at: 1_100
    })
    expect(keep(session, name)).toBeNull()
    const matched = keep(session, services)
    expect(matched.provenance).toBe('core-merged')
    expect(matched.localName.value).toBe('Polar H10 0001')
    expect(services.localName.state).toBe('absent')
  })

  test('a services-only filter matches the advertising packet alone', () => {
    const session = new ScanEvidenceSession()
    const services = packet({ address: 'A0:9E:1A:E9:B9:3D', services: [canonicalUuid(HEART)], at: 1 })
    const matched = session.matchAdvertisement(services, candidate =>
      advertisementMatchesFilter(
        { serviceUuids: [canonicalUuid(HEART)], manufacturerData: [], localNamePrefix: null },
        candidate
      )
    )
    expect(matched).toBe(services)
  })

  test('a missing name is not invented, and a different address does not merge', () => {
    const session = new ScanEvidenceSession()
    const services = packet({ address: 'A0:9E:1A:E9:B9:3D', services: [canonicalUuid(HEART)], at: 1 })
    const other = packet({ address: 'B0:9E:1A:E9:B9:3D', name: 'Polar H10 0001', at: 2 })
    expect(keep(session, services)).toBeNull()
    expect(keep(session, other)).toBeNull()
  })

  test('evidence outside the window and a cleared generation do not match', () => {
    const session = new ScanEvidenceSession()
    const services = packet({ address: 'A0:9E:1A:E9:B9:3D', services: [canonicalUuid(HEART)], at: 0 })
    const late = packet({
      address: 'A0:9E:1A:E9:B9:3D',
      name: 'Polar H10 0001',
      at: SCAN_EVIDENCE_WINDOW_MS + 1
    })
    expect(keep(session, services)).toBeNull()
    expect(keep(session, late)).toBeNull()
    const fresh = new ScanEvidenceSession()
    const again = packet({ address: 'A0:9E:1A:E9:B9:3D', services: [canonicalUuid(HEART)], at: 10 })
    const name = packet({ address: 'A0:9E:1A:E9:B9:3D', name: 'Polar H10 0001', at: 11 })
    expect(keep(fresh, again)).toBeNull()
    expect(keep(fresh, name)).not.toBeNull()
    fresh.clear()
    const staleName = packet({ address: 'A0:9E:1A:E9:B9:3D', name: 'Polar H10 0001', at: 12 })
    expect(keep(fresh, staleName)).toBeNull()
  })

  test('the public filter accepts the split pair and keeps a single-packet miss', async () => {
    const address = 'A0:9E:1A:E9:B9:3D'
    const query = normalizeScanQuery({
      anyOf: [{ services: { all: [0x180d] }, names: { prefixes: ['Polar H10'] } }]
    })
    const source = {
      limits: {
        itemCapacity: capacity(8),
        byteCapacity: capacity(4096),
        reservedControlCapacity: capacity(1)
      },
      overflowPolicy: 'suspend',
      [Symbol.asyncIterator]() {
        const values = [
          packet({ address, services: [canonicalUuid(HEART)], at: 1 }),
          packet({ address, name: 'Polar H10 0001', at: 2 })
        ]
        let index = 0
        return {
          async next() {
            if (index >= values.length) return { done: true, value: undefined }
            const value = values[index]
            index += 1
            return { done: false, value: { kind: 'value', value } }
          },
          async return() {
            index = values.length
            return { done: true, value: undefined }
          },
          [Symbol.asyncIterator]() {
            return this
          }
        }
      },
      close() {
        return Promise.resolve({ state: 'released', failures: [] })
      }
    }
    const filtered = filterScanObservations(source, query)
    const iterator = filtered[Symbol.asyncIterator]()
    const first = await iterator.next()
    expect(first.done).toBe(false)
    expect(first.value.kind).toBe('value')
    expect(first.value.value.provenance).toBe('core-merged')
    expect(first.value.value.localName).toBe('Polar H10 0001')
    expect(first.value.value.serviceUuids).toEqual([canonicalUuid(HEART)])
    const rest = await iterator.next()
    expect(rest.done).toBe(true)
  })

  test('an IPC filter ages split packets from the receipt clock', async () => {
    const address = 'A0:9E:1A:E9:B9:3D'
    const query = normalizeScanQuery({
      anyOf: [{ services: { all: [0x180d] }, names: { prefixes: ['Polar H10'] } }]
    })
    const ipcPacket = ({ name, services, address: packetAddress = address }) =>
      Object.freeze({
        peerId: 'peer-1',
        address: packetAddress,
        addressType: 'public',
        localName: name,
        rssi: null,
        txPowerLevel: null,
        serviceUuids: Object.freeze(services),
        manufacturerData: Object.freeze([]),
        serviceData: Object.freeze([])
      })
    const values = [
      ipcPacket({ name: null, services: [canonicalUuid(HEART)] }),
      ipcPacket({ name: 'Polar H10 0001', services: [] })
    ]
    const sourceFor = clockSteps => {
      let index = 0
      return {
        limits: {
          itemCapacity: capacity(8),
          byteCapacity: capacity(4096),
          reservedControlCapacity: capacity(1)
        },
        overflowPolicy: 'suspend',
        [Symbol.asyncIterator]() {
          return {
            async next() {
              if (index >= values.length) return { done: true, value: undefined }
              const value = values[index]
              index += 1
              return { done: false, value: { kind: 'value', value } }
            },
            async return() {
              index = values.length
              return { done: true, value: undefined }
            },
            [Symbol.asyncIterator]() {
              return this
            }
          }
        },
        close() {
          return Promise.resolve({ state: 'released', failures: [] })
        },
        clockSteps
      }
    }
    const read = async clockSteps => {
      let step = 0
      const filtered = filterScanObservations(
        sourceFor(clockSteps),
        query,
        'all',
        () => clockSteps[step++] ?? clockSteps.at(-1)
      )
      return filtered[Symbol.asyncIterator]().next()
    }
    const merged = await read([1_000, 1_000 + SCAN_EVIDENCE_WINDOW_MS])
    expect(merged.done).toBe(false)
    expect(merged.value.value.provenance).toBe('core-merged')
    expect(merged.value.value.localName).toBe('Polar H10 0001')
    expect(merged.value.value.serviceUuids).toEqual([canonicalUuid(HEART)])
    const expired = await read([0, SCAN_EVIDENCE_WINDOW_MS + 1])
    expect(expired.done).toBe(true)
  })
})
