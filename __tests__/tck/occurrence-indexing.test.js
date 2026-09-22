// __tests__/tck/occurrence-indexing.test.js
//
// The TCK's reading of discovery paths (docs/UNIFIED_SEMANTICS.md §9): an
// occurrence is the index of a repeated UUID under its parent, so every key
// carries the UUID at every level.

const { inspectOccurrenceIndexing } = require('../../src/tck/runner-public-occurrence-support')

const HEART_RATE = '0000180d-0000-1000-8000-00805f9b34fb'
const BATTERY = '0000180f-0000-1000-8000-00805f9b34fb'
const MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
const LEVEL = '00002a19-0000-1000-8000-00805f9b34fb'
const USER_DESCRIPTION = '00002901-0000-1000-8000-00805f9b34fb'

const service = (serviceUuid, serviceOccurrence) => ({ path: { serviceUuid, serviceOccurrence } })
const characteristic = (serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence) => ({
  path: { serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence },
  properties: { notify: true }
})
const descriptor = (parent, descriptorUuid, descriptorOccurrence) => ({
  path: { ...parent.path, descriptorUuid, descriptorOccurrence }
})

describe('TCK occurrence indexing', () => {
  test('distinct UUIDs under one parent each start at occurrence 0 and are distinct paths', () => {
    const measurement = characteristic(HEART_RATE, '0', MEASUREMENT, '0')
    const level = characteristic(BATTERY, '0', LEVEL, '0')
    const indexing = inspectOccurrenceIndexing({
      services: [service(HEART_RATE, '0'), service(BATTERY, '0')],
      characteristics: [measurement, level],
      descriptors: [descriptor(measurement, USER_DESCRIPTION, '0'), descriptor(level, USER_DESCRIPTION, '0')]
    })
    expect(indexing).toMatchObject({
      pathsUnique: true,
      parentsResolve: true,
      occurrencesExact: true,
      distinctServiceUuids: 2
    })
  })

  test('a repeated UUID is numbered per parent in discovery order at every level', () => {
    const first = characteristic(BATTERY, '0', LEVEL, '0')
    const second = characteristic(BATTERY, '0', LEVEL, '1')
    const otherService = characteristic(BATTERY, '1', LEVEL, '0')
    const indexing = inspectOccurrenceIndexing({
      services: [service(HEART_RATE, '0'), service(BATTERY, '0'), service(BATTERY, '1')],
      characteristics: [first, second, otherService],
      descriptors: [descriptor(first, USER_DESCRIPTION, '0'), descriptor(first, USER_DESCRIPTION, '1')]
    })
    expect(indexing).toMatchObject({
      pathsUnique: true,
      parentsResolve: true,
      occurrencesExact: true,
      maximumServiceOccurrence: 1,
      maximumCharacteristicOccurrence: 1,
      maximumDescriptorOccurrence: 1
    })
  })

  test('two instances of one UUID reported under one occurrence are not unique paths', () => {
    const indexing = inspectOccurrenceIndexing({
      services: [service(BATTERY, '0'), service(BATTERY, '0')],
      characteristics: [],
      descriptors: []
    })
    expect(indexing.pathsUnique).toBe(false)
    expect(indexing.occurrencesExact).toBe(false)
  })

  test('occurrences counted across UUIDs instead of per UUID are not exact', () => {
    const indexing = inspectOccurrenceIndexing({
      services: [service(HEART_RATE, '0'), service(BATTERY, '1')],
      characteristics: [],
      descriptors: []
    })
    expect(indexing.pathsUnique).toBe(true)
    expect(indexing.occurrencesExact).toBe(false)
  })

  test('a characteristic whose service was not discovered does not resolve', () => {
    const indexing = inspectOccurrenceIndexing({
      services: [service(HEART_RATE, '0')],
      characteristics: [characteristic(BATTERY, '0', LEVEL, '0')],
      descriptors: []
    })
    expect(indexing.parentsResolve).toBe(false)
  })
})
