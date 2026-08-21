const {
  normalizeScanQuery,
  normalizeScanObservation,
  observationMatchesScanQuery
} = require('../src/public/scan-query')
const { filterScanObservations, findPeerInScan } = require('../src/public/ble-manager')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')

function observation(overrides = {}) {
  return {
    localName: 'Heart Strap',
    rssi: -42,
    connectable: true,
    serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb', '0000180f-0000-1000-8000-00805f9b34fb'],
    manufacturerData: [
      { companyId: 76, data: new Uint8Array([2, 21, 1, 2]) },
      { companyId: 123, data: new Uint8Array([9, 8]) }
    ],
    serviceData: [{ service: '0000180d-0000-1000-8000-00805f9b34fb', data: new Uint8Array([1, 2, 3]) }],
    ...overrides
  }
}

describe('canonical public ScanQuery v1', () => {
  test('normalizes omitted match-all, rejects ambiguous empty/unknown shapes, and freezes bytes', () => {
    const query = normalizeScanQuery()
    expect(query.anyOf).toBeNull()
    expect(query.exclude).toBeNull()
    expect(Object.isFrozen(query)).toBe(true)
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation()))).toBe(true)

    expect(() => normalizeScanQuery({ anyOf: [] })).toThrow()
    expect(() => normalizeScanQuery({ exclude: [] })).toThrow()
    expect(() => normalizeScanQuery({ anyOf: [{ peers: ['peer'] }] })).toThrow()

    const prefix = new Uint8Array([2])
    const normalized = normalizeScanQuery({
      anyOf: [{ manufacturerData: { any: [{ companyId: 76, dataPrefix: prefix }] } }]
    })
    prefix[0] = 9
    expect([...normalized.anyOf[0].manufacturerData.any[0].dataPrefix]).toEqual([2])
  })

  test('applies clause AND, anyOf OR, field any/all, and exclusion precedence', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          services: { all: ['180d', '180f'] },
          names: { prefixes: ['Heart'] },
          manufacturerData: { all: [{ companyId: 76, dataPrefix: new Uint8Array([2]) }, { companyId: 123 }] },
          serviceData: { any: [{ service: '180d', dataPrefix: new Uint8Array([1, 2]) }] },
          rssi: { minimum: -50, maximum: -30 },
          connectable: true
        },
        { names: { exact: ['Other'] } }
      ],
      exclude: [{ services: { any: ['180f'] } }]
    })
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation()))).toBe(false)
    expect(
      observationMatchesScanQuery(
        normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Heart'] } }], exclude: [{ connectable: false }] }),
        normalizeScanObservation(observation())
      )
    ).toBe(true)
  })

  test('uses masked byte matching and distinguishes absent fields from empty observed data', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          manufacturerData: {
            any: [{ companyId: 76, dataPrefix: new Uint8Array([0x02, 0x10]), mask: new Uint8Array([0xff, 0xf0]) }]
          },
          serviceData: {
            all: [{ service: '180d', mask: new Uint8Array([0xff, 0x0f]), dataPrefix: new Uint8Array([1, 2]) }]
          }
        }
      ]
    })
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation()))).toBe(true)
    expect(
      observationMatchesScanQuery(
        query,
        normalizeScanObservation(observation({ manufacturerData: null, serviceData: null }))
      )
    ).toBe(false)
    expect(() =>
      normalizeScanQuery({ anyOf: [{ manufacturerData: { any: [{ companyId: 1, mask: new Uint8Array([1]) }] } }] })
    ).toThrow()
    expect(() => normalizeScanQuery({ anyOf: [{ rssi: { minimum: 0, maximum: -1 } }] })).toThrow()
  })

  test('produces an order-independent semantic digest and canonical UUIDs', () => {
    const first = normalizeScanQuery({
      anyOf: [{ services: { any: ['180F', '180D', '180D'] }, names: { exact: ['A', 'B'] } }]
    })
    const second = normalizeScanQuery({
      anyOf: [{ names: { exact: ['A', 'B'] }, services: { any: ['180d', '180f'] } }]
    })
    expect(first.digest).toBe(second.digest)
    expect(first.anyOf[0].services.any).toEqual([
      '0000180d-0000-1000-8000-00805f9b34fb',
      '0000180f-0000-1000-8000-00805f9b34fb'
    ])
    expect(first.digest).toMatch(/^scan-query-v1:[0-9a-f]{16}$/)
    const clauses = [{ names: { prefixes: ['Target'] } }, { rssi: { minimum: -60 } }]
    expect(normalizeScanQuery({ anyOf: clauses }).digest).toBe(
      normalizeScanQuery({ anyOf: [...clauses].reverse() }).digest
    )
  })

  test('uses the same normalized query for the public residual stream and find helper', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const filtered = filterScanObservations(
      source,
      normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Target'] } }] })
    )
    const iterator = filtered[Symbol.asyncIterator]()
    const pending = iterator.next()
    source.emit(
      {
        peerId: 'ignored',
        localName: 'Other',
        rssi: -80,
        txPowerLevel: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )
    source.emit(
      {
        peerId: 'target',
        localName: 'Target Device',
        rssi: -40,
        txPowerLevel: null,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      32
    )
    await expect(pending).resolves.toMatchObject({ value: { kind: 'value', value: { peer: { id: 'target' } } } })
    await filtered.close()

    const scan = {
      observations: filtered,
      stop: async () => ({ state: 'released', failures: [] })
    }
    await expect(findPeerInScan(scan, 'first')).rejects.toMatchObject({ code: 'stream.closed' })
  })
})
