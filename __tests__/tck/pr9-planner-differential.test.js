const { normalizeScanObservation, normalizeScanQuery } = require('../../src/public/scan-query')
const { bluezScanPlanningContext, BluezScanPlanner } = require('../../src/backends/bluez/bluez-scan-planner')
const {
  coreBluetoothScanPlanningContext,
  CoreBluetoothScanPlanner
} = require('../../src/backends/corebluetooth/corebluetooth-scan-planner')
const { winRtScanPlanningContext, WinRtScanPlanner } = require('../../src/backends/winrt/winrt-scan-planner')
const { runPlannerDifferentialTck, MAX_PLANNER_DIFFERENTIAL_SCENARIOS } = require('../../src/tck/planner-differential')
const vectors = require('../backend-contract/fixtures/scan-query-pr9-planner.golden.json')

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

function nativeAccepts(filter, observation) {
  return (
    filter.serviceUuids.length === 0 ||
    (observation.serviceUuids !== null && filter.serviceUuids.every(uuid => observation.serviceUuids.includes(uuid)))
  )
}

describe('PR9 planner differential TCK', () => {
  test.each([
    ['bluez', new BluezScanPlanner(), bluezScanPlanningContext],
    ['corebluetooth', new CoreBluetoothScanPlanner(), coreBluetoothScanPlanningContext],
    ['winrt', new WinRtScanPlanner(), winRtScanPlanningContext]
  ])(
    '%s proves native superset and residual equivalence for every bounded golden scenario',
    (_id, planner, context) => {
      const report = runPlannerDifferentialTck({
        planner,
        context,
        scenarios: vectors.map(vector => ({
          id: vector.id,
          query: hydrate(vector.query),
          observation: hydrate(vector.observation),
          expectedMatch: vector.expectedMatch
        })),
        normalizeQuery: normalizeScanQuery,
        normalizeObservation: normalizeScanObservation,
        nativeAccepts
      })

      expect(report.scenarioCount).toBe(vectors.length)
      expect(report.facts).toEqual(
        expect.arrayContaining([
          expect.objectContaining({ id: 'planner-native-projection-is-safe-superset', holds: true }),
          expect.objectContaining({ id: 'planner-residual-matcher-is-differentially-equivalent', holds: true }),
          expect.objectContaining({ id: 'planner-diagnostics-are-bounded-and-payload-free', holds: true })
        ])
      )
    }
  )

  test('rejects an unbounded scenario set before evaluating planner behavior', () => {
    expect(() =>
      runPlannerDifferentialTck({
        planner: new BluezScanPlanner(),
        context: bluezScanPlanningContext,
        scenarios: Array.from({ length: MAX_PLANNER_DIFFERENTIAL_SCENARIOS + 1 }, (_, index) => ({
          id: `scenario-${index}`,
          query: {},
          observation: {
            peerId: `peer-${index}`,
            localName: null,
            rssi: null,
            txPowerLevel: null,
            serviceUuids: [],
            manufacturerData: [],
            serviceData: []
          },
          expectedMatch: true
        })),
        normalizeQuery: normalizeScanQuery,
        normalizeObservation: normalizeScanObservation,
        nativeAccepts
      })
    ).toThrow('bounded at')
  })
})
