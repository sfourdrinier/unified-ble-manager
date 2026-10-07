'use strict'

const { advertisementMatchesFilter } = require('../../src/backend-contract/advertisement')
const { canonicalUuid, capacity, monotonicTimestamp, opaqueId } = require('../../src/backend-contract/primitives')
const {
  ScanEvidenceSession,
  SCAN_EVIDENCE_WINDOW_MS,
  SCAN_EVIDENCE_PEER_CAPACITY
} = require('../../src/backend-contract/scan-evidence')
const { filterScanObservations } = require('../../src/public/ble-manager')
const { normalizeScanQuery } = require('../../src/public/scan-query')

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
  backend = 'backend'
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
    sourceTimestamp: absent(),
    receivedAtMonotonicMs: monotonicTimestamp(at),
    ingressOrdinal: 1,
    scanSessionId: opaqueId(session, 'scan-session', 'test'),
    localName: name === null ? absent() : present(name),
    rssi: absent(),
    txPower: absent(),
    connectable: absent(),
    appearance: absent(),
    serviceUuids: services === null ? present(Object.freeze([])) : present(Object.freeze(services)),
    solicitedServiceUuids: absent(),
    overflowServiceUuids: absent(),
    serviceData: present(Object.freeze([])),
    manufacturerData: present(Object.freeze([])),
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
