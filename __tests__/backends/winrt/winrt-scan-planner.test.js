const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../../src/public/scan-query')
const { WinRtBackend } = require('../../../src/backends/winrt/winrt-backend')
const {
  WinRtScanPlanner,
  diagnosticWinRtScanPlan,
  winRtScanPlanningContext
} = require('../../../src/backends/winrt/winrt-scan-planner')
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

describe('WinRT scan planner', () => {
  test('advertises only observation fields supplied by the WinRT watcher', () => {
    expect(winRtScanPlanningContext.availableObservationFields).not.toContain('connectable')
  })

  test('publishes the diagnostic planner through the backend scanner hook', () => {
    const adapterState = { availability: 'available', authorization: 'granted', power: 'on', safeReason: null }
    const backend = new WinRtBackend(
      {
        adapterSnapshot: () => adapterState,
        onConnectionLost: () => () => undefined,
        onDatabaseChanged: () => () => undefined,
        onScanTerminal: () => () => undefined,
        onAdapterState: () => () => undefined
      },
      { nativeAdapterId: 'adapter', displayName: 'adapter', state: adapterState, deployment: 'unpackaged' },
      () => 1,
      'node'
    )

    expect(backend.scanner.plan(normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] }))).toMatchObject({
      nativeGuarantee: 'safe-superset'
    })
  })

  test('projects only services required by every positive clause through the proven watcher filter', () => {
    const query = normalizeScanQuery({
      anyOf: [{ services: { all: ['180d', '180f'] } }, { services: { all: ['180d'] } }]
    })

    const execution = new WinRtScanPlanner().plan(query, winRtScanPlanningContext)

    expect(execution.nativeFilter).toEqual({
      serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
      manufacturerData: [],
      localNamePrefix: null
    })
    expect(execution.nativeGuarantee).toBe('safe-superset')
    expect(execution.residual.query).toEqual(query)
    expect(execution.residual.complete).toBe(true)
  })

  test('marks advertisement fields absent from WinRT unavailable while retaining every residual predicate', () => {
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

    const execution = new WinRtScanPlanner().plan(query, winRtScanPlanningContext)

    expect(execution.unavailable).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'connectable', operator: 'equals' },
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'manufacturerData', operator: 'any' },
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'serviceData', operator: 'any' }
    ])
    expect(execution.residual.query).toEqual(query)
    expect(execution.residual.predicates).toEqual(
      expect.arrayContaining([
        { clauseSet: 'anyOf', clauseIndex: 0, field: 'manufacturerData', operator: 'any' },
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

  test('does not project unsafe service UUID shapes', () => {
    const inputs = [
      {},
      { anyOf: [{ services: { any: ['180d'] } }] },
      { anyOf: [{ services: { all: ['180d'] } }, { services: { all: ['180f'] } }] },
      { exclude: [{ services: { any: ['180d'] } }] }
    ]

    for (const input of inputs) {
      const execution = new WinRtScanPlanner().plan(normalizeScanQuery(input), winRtScanPlanningContext)
      expect(execution.nativeFilter.serviceUuids).toEqual([])
    }
  })

  test('returns a defensive diagnostic snapshot', () => {
    const plan = diagnosticWinRtScanPlan(normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] }))

    expect(plan.native).toEqual({
      predicates: [{ clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' }],
      complete: false
    })
    expect(plan.residual.query).toEqual(plan.sourceQuery)
    expect(Object.isFrozen(plan)).toBe(true)
  })

  test('keeps every canonical match in the WinRT native superset', () => {
    for (const vector of vectors) {
      const query = normalizeScanQuery(hydrate(vector.query))
      const observation = normalizeScanObservation(hydrate(vector.observation))
      const execution = new WinRtScanPlanner().plan(query, winRtScanPlanningContext)
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
