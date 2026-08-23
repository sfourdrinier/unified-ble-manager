const {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery
} = require('../../src/public/scan-query')
const { bluezScanPlanningContext, BluezScanPlanner } = require('../../src/backends/bluez/bluez-scan-planner')
const {
  coreBluetoothScanPlanningContext,
  CoreBluetoothScanPlanner
} = require('../../src/backends/corebluetooth/corebluetooth-scan-planner')
const { winRtScanPlanningContext, WinRtScanPlanner } = require('../../src/backends/winrt/winrt-scan-planner')
const {
  reactNativeAndroidScanPlanningContext,
  reactNativeAppleScanPlanningContext,
  ReactNativeScanPlanner
} = require('../../src/backends/reactnative/react-native-scan-planner')
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
    ['winrt', new WinRtScanPlanner(), winRtScanPlanningContext],
    ['react-native-android', new ReactNativeScanPlanner(), reactNativeAndroidScanPlanningContext],
    ['react-native-apple', new ReactNativeScanPlanner(), reactNativeAppleScanPlanningContext]
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

  test('covers deterministic generated observations and detects a mutated native matcher', () => {
    const generated = Array.from({ length: 16 }, (_, seed) => {
      const query =
        seed % 2 === 0
          ? { anyOf: [{ services: { all: ['180d'] }, names: { prefixes: ['Heart'] } }] }
          : { exclude: [{ services: { any: ['180f'] } }] }
      const observation = {
        peerId: `generated-peer-${seed}`,
        localName: seed % 3 === 0 ? 'Heart Strap' : 'Other Sensor',
        rssi: -40 - seed,
        txPowerLevel: null,
        serviceUuids:
          seed % 4 === 0 ? ['0000180d-0000-1000-8000-00805f9b34fb'] : ['0000180f-0000-1000-8000-00805f9b34fb'],
        manufacturerData: [],
        serviceData: []
      }
      const normalizedQuery = normalizeScanQuery(query)
      const normalizedObservation = normalizeScanObservation(observation)
      return {
        id: `generated-${seed}`,
        query,
        observation,
        expectedMatch: observationMatchesScanQuery(normalizedQuery, normalizedObservation)
      }
    })
    const report = runPlannerDifferentialTck({
      planner: new BluezScanPlanner(),
      context: bluezScanPlanningContext,
      scenarios: generated,
      normalizeQuery: normalizeScanQuery,
      normalizeObservation: normalizeScanObservation,
      nativeAccepts
    })
    expect(report.facts).toEqual(expect.arrayContaining([expect.objectContaining({ holds: true })]))

    const mutated = runPlannerDifferentialTck({
      planner: new BluezScanPlanner(),
      context: bluezScanPlanningContext,
      scenarios: generated,
      normalizeQuery: normalizeScanQuery,
      normalizeObservation: normalizeScanObservation,
      nativeAccepts: filter => filter.serviceUuids.length === 0
    })
    expect(mutated.facts.find(fact => fact.id === 'planner-native-projection-is-safe-superset').holds).toBe(false)
  })
})
