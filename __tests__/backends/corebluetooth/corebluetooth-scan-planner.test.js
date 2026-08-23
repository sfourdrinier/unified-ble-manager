const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../../src/public/scan-query')
const { CoreBluetoothBackend } = require('../../../src/backends/corebluetooth/corebluetooth-backend')
const {
  CoreBluetoothScanPlanner,
  coreBluetoothScanPlanningContext,
  diagnosticCoreBluetoothScanPlan
} = require('../../../src/backends/corebluetooth/corebluetooth-scan-planner')
const vectors = require('../../backend-contract/fixtures/scan-query-pr9-planner.golden.json')

function hydrate(value) {
  if (value !== null && typeof value === 'object' && value.$bytes !== undefined) {
    return new Uint8Array(value.$bytes)
  }
  if (Array.isArray(value)) return value.map(item => hydrate(item))
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, entry]) => [key, hydrate(entry)]))
  }
  return value
}

describe('CoreBluetooth scan planner', () => {
  test('publishes the diagnostic planner through the backend scanner hook', () => {
    const backend = new CoreBluetoothBackend(
      {
        adapterSnapshot: () => ({ availability: 'available', authorization: 'granted', power: 'on', safeReason: null }),
        onDisconnect: () => () => undefined,
        onAdapterState: () => () => undefined
      },
      () => 1,
      'node'
    )

    expect(backend.scanner.plan(normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] }))).toMatchObject({
      nativeGuarantee: 'safe-superset'
    })
  })

  test('projects only services required by every positive clause', () => {
    const query = normalizeScanQuery({
      anyOf: [{ services: { all: ['180d', '180f'] } }, { services: { all: ['180d'] } }]
    })

    const execution = new CoreBluetoothScanPlanner().plan(query, coreBluetoothScanPlanningContext)

    expect(execution.nativeFilter).toEqual({
      serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
      manufacturerData: [],
      localNamePrefix: null
    })
    expect(execution.nativeGuarantee).toBe('safe-superset')
    expect(execution.residual.query).toEqual(query)
    expect(execution.residual.complete).toBe(true)
  })

  test('reports native, residual, and unavailable predicates without weakening the residual', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          services: { all: ['180d'] },
          names: { prefixes: ['Polar'] },
          manufacturerData: { any: [{ companyId: 1, dataPrefix: new Uint8Array([1]) }] },
          serviceData: { any: [{ service: '180d', dataPrefix: new Uint8Array([2]) }] },
          rssi: { minimum: -70 },
          connectable: true
        }
      ]
    })
    const context = {
      ...coreBluetoothScanPlanningContext,
      availableObservationFields: coreBluetoothScanPlanningContext.availableObservationFields.filter(
        field => field !== 'serviceData'
      )
    }

    const execution = new CoreBluetoothScanPlanner().plan(query, context)

    expect(execution.native.predicates).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' }
    ])
    expect(execution.unavailable).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'serviceData', operator: 'any' }
    ])
    expect(execution.residual.query).toEqual(query)
    expect(execution.residual.predicates).toEqual(
      expect.arrayContaining([
        { clauseSet: 'anyOf', clauseIndex: 0, field: 'names', operator: 'prefixes' },
        { clauseSet: 'anyOf', clauseIndex: 0, field: 'serviceData', operator: 'any' }
      ])
    )
    expect(execution.limitations).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ code: 'native-filter-incomplete', effect: 'performance-only' }),
        expect.objectContaining({ code: 'observation-field-unavailable', effect: 'field-unavailable' })
      ])
    )
  })

  test('returns a defensive diagnostic snapshot', () => {
    const plan = diagnosticCoreBluetoothScanPlan(normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] }))

    expect(plan.native).toEqual({
      predicates: [{ clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' }],
      complete: false
    })
    expect(plan.residual.query).toEqual(plan.sourceQuery)
    expect(Object.isFrozen(plan)).toBe(true)
  })

  test('keeps every canonical match in the CoreBluetooth native superset', () => {
    for (const vector of vectors) {
      const query = normalizeScanQuery(hydrate(vector.query))
      const observation = normalizeScanObservation(hydrate(vector.observation))
      const execution = new CoreBluetoothScanPlanner().plan(query, coreBluetoothScanPlanningContext)
      const nativeAccepts =
        execution.nativeFilter.serviceUuids.length === 0 ||
        (observation.serviceUuids !== null &&
          execution.nativeFilter.serviceUuids.every(uuid => observation.serviceUuids.includes(uuid)))
      const optimizedMatch = nativeAccepts && observationMatchesScanQuery(execution.residual.query, observation)

      expect(optimizedMatch).toBe(vector.expectedMatch)
      if (vector.expectedMatch) expect(nativeAccepts).toBe(true)
    }
  })
})
