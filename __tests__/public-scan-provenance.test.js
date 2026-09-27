const { normalizeScanObservation } = require('../src/public/scan-query')
const { snapshotAdvertisementObservation } = require('../src/electron/advertisement-observation')
const { deterministicScenarioAdvertisement } = require('../src/testing/scenarios/manager-scenario-executor')
const { filterScanObservations } = require('../src/public/ble-manager')
const { normalizeScanQuery } = require('../src/public/scan-query')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
const { awaitSignal } = require('./helpers/async')

test('coalescing preserves an origin-only change', async () => {
  const source = new CoreBoundedStream(
    { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  const iterator = filterScanObservations(source, normalizeScanQuery(), 'coalesced')[Symbol.asyncIterator]()
  const observation = {
    peerId: 'peer',
    localName: null,
    rssi: null,
    serviceUuids: [],
    manufacturerData: [],
    serviceData: []
  }
  for (const origin of ['device-state', 'advertisement']) {
    const next = iterator.next()
    source.emit({ ...observation, origin }, 64)
    await expect(awaitSignal(next, 'origin-only scan change')).resolves.toMatchObject({
      value: { kind: 'value', value: { origin } }
    })
  }
  await iterator.return()
})

test.each(['advertisement', 'device-state'])('public and Electron projections retain exact %s origin', origin => {
  const native = { ...deterministicScenarioAdvertisement(), provenance: 'platform-derived', origin }
  const direct = normalizeScanObservation(native)
  expect(direct).toMatchObject({ provenance: 'platform-derived', origin })
  const ipc = snapshotAdvertisementObservation(native)
  expect(ipc).toMatchObject({ provenance: 'platform-derived', origin })
  expect(normalizeScanObservation(ipc)).toEqual(direct)
  expect(normalizeScanObservation(direct)).toEqual(direct)
  expect(direct.serviceUuids).toEqual(native.serviceUuids.value)
})

test('legacy observations preserve provenance without inventing exact origin', () => {
  const native = deterministicScenarioAdvertisement()
  const result = normalizeScanObservation(native)
  expect(result.provenance).toBe(native.provenance)
  expect(result).not.toHaveProperty('origin')
  const compact = {
    peerId: 'peer',
    localName: null,
    rssi: null,
    serviceUuids: [],
    manufacturerData: [],
    serviceData: []
  }
  expect(normalizeScanObservation(compact)).not.toHaveProperty('origin')
  expect(normalizeScanObservation(compact)).not.toHaveProperty('provenance')
  expect(
    normalizeScanObservation({ ...compact, origin: 'device-state', provenance: 'platform-derived' })
  ).toMatchObject({ origin: 'device-state', provenance: 'platform-derived' })
})

test.each(['rumour', null, 1])('unknown origin %p is rejected at native/public/IPC boundaries', origin => {
  const native = { ...deterministicScenarioAdvertisement(), origin }
  expect(() => normalizeScanObservation(native)).toThrow()
  expect(() => snapshotAdvertisementObservation(native)).toThrow()
  const normalized = normalizeScanObservation(deterministicScenarioAdvertisement())
  expect(() => normalizeScanObservation({ ...normalized, origin })).toThrow()
})
