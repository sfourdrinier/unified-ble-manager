const {
  normalizeScanQuery,
  normalizeScanObservation,
  observationMatchesScanQuery
} = require('../../../src/public/scan-query')
const {
  planReactNativeAndroidScan,
  planReactNativeAppleScan
} = require('../../../src/backends/reactnative/react-native-scan-planner')
const {
  CoreBluetoothScanPlanner,
  coreBluetoothScanPlanningContext
} = require('../../../src/backends/corebluetooth/corebluetooth-scan-planner')

const hr = '0000180d-0000-1000-8000-00805f9b34fb'
const { BluezScanPlanner, bluezScanPlanningContext } = require('../../../src/backends/bluez/bluez-scan-planner')
const { WinRtScanPlanner, winRtScanPlanningContext } = require('../../../src/backends/winrt/winrt-scan-planner')
const planners = [
  ['Android', planReactNativeAndroidScan],
  ['Apple', planReactNativeAppleScan],
  ['desktop CoreBluetooth', query => new CoreBluetoothScanPlanner().plan(query, coreBluetoothScanPlanningContext)],
  ['BlueZ', query => new BluezScanPlanner().plan(query, bluezScanPlanningContext)],
  ['WinRT', query => new WinRtScanPlanner().plan(query, winRtScanPlanningContext)]
]

describe.each(planners)('%s singleton service planning', (_name, plan) => {
  test.each(['exact', 'prefixes'])('pushes singleton any but retains %s name matching', operator => {
    const query = normalizeScanQuery({ anyOf: [{ services: { any: ['180d'] }, names: { [operator]: ['Polar'] } }] })
    const result = plan(query)
    expect(result.nativeFilter.serviceUuids).toEqual([hr])
    expect(result.nativeFilter.localNamePrefix).toBeNull()
    expect(result.native.predicates).toEqual([
      { clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'any' }
    ])
    expect(result.residual.query).toEqual(query)
    expect(result.residual.complete).toBe(true)
  })

  test.each([
    [[{ services: { any: ['180d'] } }, { services: { all: ['180d', '180f'] } }], [hr]],
    [[{ services: { any: ['180d'], all: ['180f'] } }, { services: { all: ['180d'] } }], [hr]],
    [[{ services: { any: ['180d'] } }, { services: { any: ['180f'] } }], []],
    [[{ services: { any: ['180d'] } }, { names: { exact: ['Other'] } }], []],
    [[{ services: { any: ['180d', '180f'] } }], []],
    [[{ services: { any: ['180d', '180f'], all: ['180d'] } }], [hr]]
  ])('keeps OR branches a safe superset: %j', (anyOf, expected) => {
    expect(plan(normalizeScanQuery({ anyOf })).nativeFilter.serviceUuids).toEqual(expected)
  })

  test('does not derive a positive filter from exclusions', () => {
    expect(plan(normalizeScanQuery({ exclude: [{ services: { any: ['180d'] } }] })).nativeFilter.serviceUuids).toEqual(
      []
    )
  })

  test('shared broadening does not lose matches and each member retains its residual', () => {
    const queries = [
      normalizeScanQuery({ anyOf: [{ services: { any: ['180d'] }, names: { exact: ['Polar'] } }] }),
      normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Other'] } }] })
    ]
    const executions = queries.map(plan)
    // The native membership union must be broad when any member is broad.
    expect(executions.map(result => result.nativeFilter.serviceUuids)).toEqual([[hr], []])
    for (const [localName, services, expected] of [
      ['Polar', ['180d'], [true, false]],
      ['Other', ['180f'], [false, true]],
      ['Wrong', ['180d'], [false, false]],
      ['Polar', ['180f'], [false, false]]
    ]) {
      const observation = normalizeScanObservation({
        localName,
        serviceUuids: services.map(service => `0000${service}-0000-1000-8000-00805f9b34fb`),
        rssi: -50,
        connectable: true,
        manufacturerData: [],
        serviceData: []
      })
      expect(executions.map(result => observationMatchesScanQuery(result.residual.query, observation))).toEqual(
        expected
      )
    }
  })
})
