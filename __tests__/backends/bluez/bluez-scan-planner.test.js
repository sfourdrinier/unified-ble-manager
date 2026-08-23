const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../../src/public/scan-query')
const {
  BluezScanPlanner,
  bluezScanPlanningContext,
  diagnosticBluezScanPlan
} = require('../../../src/backends/bluez/bluez-scan-planner')
const { scanFilterVariant } = require('../../../src/backends/bluez/bluez-runtime-models')
const vectors = require('../../backend-contract/fixtures/scan-query-pr9-planner.golden.json')

const context = {
  backendId: 'bluez',
  platformId: 'bluez-test',
  availableObservationFields: ['peerReference', 'localName', 'rssi', 'connectable', 'serviceUuids']
}

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

  test('describes native, residual, and unavailable predicates truthfully', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          services: { all: ['180d'] },
          names: { prefixes: ['Polar'] },
          connectable: true
        }
      ]
    })
    const execution = new BluezScanPlanner().plan(query, {
      ...context,
      availableObservationFields: context.availableObservationFields.filter(field => field !== 'connectable')
    })

    expect(execution.native.predicates).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' }
    ])
    expect(execution.unavailable).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'connectable', operator: 'equals' }
    ])
    expect(execution.limitations).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          code: 'native-filter-incomplete',
          predicate: { clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' },
          effect: 'performance-only'
        }),
        expect.objectContaining({
          code: 'host-predicate-restricted',
          predicate: { clauseSet: 'anyOf', clauseIndex: 0, field: 'names', operator: 'prefixes' },
          effect: 'host-restriction'
        }),
        expect.objectContaining({
          code: 'observation-field-unavailable',
          predicate: { clauseSet: 'anyOf', clauseIndex: 0, field: 'connectable', operator: 'equals' },
          effect: 'field-unavailable'
        })
      ])
    )
  })

  test('does not describe partially pushed shared service predicates as native', () => {
    const query = normalizeScanQuery({
      anyOf: [{ services: { all: ['180d', '180f'] } }, { services: { all: ['180d'] } }]
    })

    const execution = new BluezScanPlanner().plan(query, context)

    expect(execution.nativeFilter.serviceUuids).toEqual(['0000180d-0000-1000-8000-00805f9b34fb'])
    expect(execution.native.predicates).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 1, field: 'services', operator: 'all' }
    ])
  })

  test('rejects invalid planning context fields', () => {
    expect(() =>
      new BluezScanPlanner().plan(normalizeScanQuery({}), {
        ...context,
        availableObservationFields: ['not-a-field']
      })
    ).toThrow('invalid BlueZ scan planning context')
  })

  test('reports peer predicates unavailable for the actual BlueZ observation shape', () => {
    const query = normalizeScanQuery({
      anyOf: [
        {
          peers: [{ version: 1, backendId: 'bluez', scope: 'system', opaqueId: 'peer-1' }]
        }
      ]
    })
    const execution = new BluezScanPlanner().plan(query, bluezScanPlanningContext)

    expect(execution.unavailable).toContainEqual({
      clauseSet: 'anyOf',
      clauseIndex: 0,
      field: 'peers',
      operator: 'equals'
    })
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

  test('derives the trusted native filter from the host-issued source plan', () => {
    const query = normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] })
    const plan = diagnosticBluezScanPlan(query)
    const variant = scanFilterVariant({
      query,
      plan,
      filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
      duplicatePolicy: 'all'
    })

    expect(variant.value.UUIDs).toEqual({
      signature: 'as',
      value: ['0000180d-0000-1000-8000-00805f9b34fb']
    })
  })

  test('rejects a plan that is not bound to the actual scan query', () => {
    const planQuery = normalizeScanQuery({ anyOf: [{ services: { all: ['180d'] } }] })
    const actualQuery = normalizeScanQuery({ anyOf: [{ services: { all: ['180f'] } }] })
    expect(() =>
      scanFilterVariant({
        query: actualQuery,
        plan: diagnosticBluezScanPlan(planQuery),
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
        duplicatePolicy: 'all'
      })
    ).toThrow('bluez.scan.plan-query')
  })

  test('bounds diagnostics for a large valid query', () => {
    const query = normalizeScanQuery({
      anyOf: Array.from({ length: 33 }, (_, index) => ({ names: { prefixes: [`name-${index}`] } }))
    })
    const execution = new BluezScanPlanner().plan(query, context)
    expect(execution.limitations).toHaveLength(32)
  })

  test('keeps every canonical match in the BlueZ native superset', () => {
    for (const vector of vectors) {
      const query = normalizeScanQuery(hydrate(vector.query))
      const observation = normalizeScanObservation(hydrate(vector.observation))
      const execution = new BluezScanPlanner().plan(query, context)
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
