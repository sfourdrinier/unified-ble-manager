const { normalizeScanQuery } = require('../../../src/public/scan-query')
const { BluezScanPlanner } = require('../../../src/backends/bluez/bluez-scan-planner')

const context = {
  backendId: 'bluez',
  platformId: 'bluez-test',
  availableObservationFields: ['peerReference', 'localName', 'rssi', 'connectable', 'serviceUuids']
}

describe('BlueZ scan planner', () => {
  test('pushes only UUIDs required by every positive clause', () => {
    const query = normalizeScanQuery({
      anyOf: [{ services: { all: ['180d', '180f'] } }, { services: { all: ['180d'] } }]
    })
    const execution = new BluezScanPlanner().plan(query, context)

    expect(execution.nativeFilter.serviceUuids).toEqual(['0000180d-0000-1000-8000-00805f9b34fb'])
    expect(execution.nativeFilter.manufacturerData).toEqual([])
    expect(execution.nativeFilter.localNamePrefix).toBeNull()
    expect(execution.sourceQuery).toStrictEqual(execution.residual.query)
    expect(execution.nativeGuarantee).toBe('safe-superset')
  })

  test.each([
    ['no positive clauses', {}],
    ['services.any', { anyOf: [{ services: { any: ['180d'] } }] }],
    ['different required services', { anyOf: [{ services: { all: ['180d'] } }, { services: { all: ['180f'] } }] }],
    ['exclusion only', { exclude: [{ services: { any: ['180d'] } }] }]
  ])('does not push unsafe UUIDs for %s', (_label, input) => {
    const execution = new BluezScanPlanner().plan(normalizeScanQuery(input), context)
    expect(execution.nativeFilter.serviceUuids).toEqual([])
  })
})
