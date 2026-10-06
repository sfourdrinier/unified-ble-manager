'use strict'

const { advertisementMatchesFilter } = require('../../src/backend-contract/advertisement')
const { canonicalUuid, capacity, monotonicTimestamp, opaqueId } = require('../../src/backend-contract/primitives')
const { ScanEvidenceSession, SCAN_EVIDENCE_WINDOW_MS } = require('../../src/backend-contract/scan-evidence')
const { filterScanObservations } = require('../../src/public/ble-manager')
const { normalizeScanQuery } = require('../../src/public/scan-query')

const HEART = '0000180d-0000-1000-8000-00805f9b34fb'

function absent() {
  return Object.freeze({ state: 'absent', reason: 'not in this packet', provenance: 'not-provided' })
}

function present(value) {
  return Object.freeze({ state: 'present', value, provenance: 'observed' })
}

function packet({ address, name = null, services = null, at = 0, session = 'scan-1' }) {
  return Object.freeze({
    device: Object.freeze({
      id: opaqueId(`peer-${address}`, 'peer', 'test'),
      backendInstanceId: opaqueId('backend', 'backend', 'test'),
      scope: 'backend',
      stableAcrossRestarts: false,
      address: Object.freeze({ value: address, type: 'public' })
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
})
