// __tests__/public-address-targeting.test.js
//
// peer:address-targeting — the `addresses` scan clause and the address form of
// `connect()`. Backends that do not register the capability fail closed with
// `capability.unsupported`; the deterministic backend deliberately does not
// register it, which makes it the canonical unsupported host here.

const {
  normalizeScanQuery,
  normalizeScanObservation,
  observationMatchesScanQuery
} = require('../src/public/scan-query')
const { canonicalBleAddress } = require('../src/backend-contract/primitives')
const { canonicalScanQueryJson } = require('../src/backend-contract/scan-query')
const { BackendContractError } = require('../src/backend-contract/errors')
const { createDeterministicTestBleManager } = require('../src/testing/deterministic/deterministic-test-manager')

function expectCode(run, code) {
  try {
    run()
    throw new Error(`expected ${code}`)
  } catch (error) {
    expect(error).toBeInstanceOf(BackendContractError)
    expect(error.normalized.code).toBe(code)
  }
}

function observation(overrides = {}) {
  return {
    localName: 'Sensor',
    rssi: -60,
    connectable: true,
    serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
    manufacturerData: [],
    serviceData: [],
    ...overrides
  }
}

describe('canonical BLE address form', () => {
  test('canonicalizes case and separators and rejects malformed addresses', () => {
    expect(canonicalBleAddress('aa:bb:cc:dd:ee:ff')).toBe('AA:BB:CC:DD:EE:FF')
    expect(canonicalBleAddress('98-75-96-a2-14-34')).toBe('98:75:96:A2:14:34')
    expect(canonicalBleAddress('98:75:96:A2:14:34')).toBe('98:75:96:A2:14:34')
    for (const malformed of ['', '98:75:96:A2:14', '98:75:96:A2:14:34:56', 'aabbccddeeff', 'zz:75:96:a2:14:34', '98 75 96 a2 14 34']) {
      expect(() => canonicalBleAddress(malformed)).toThrow()
    }
  })
})

describe('addresses scan clause', () => {
  test('normalizes addresses to canonical form, dedupes, sorts, and stays digest-compatible', () => {
    const query = normalizeScanQuery({
      anyOf: [{ addresses: ['aa:bb:cc:dd:ee:ff', 'AA-BB-CC-DD-EE-FF', '11:22:33:44:55:66'] }]
    })
    expect(query.anyOf[0].addresses).toEqual(['11:22:33:44:55:66', 'AA:BB:CC:DD:EE:FF'])
    expect(Object.isFrozen(query.anyOf[0].addresses)).toBe(true)

    // Queries without an addresses clause keep their pre-existing canonical JSON
    // (and therefore digest): a null addresses field is excluded like null peers.
    const withoutAddresses = normalizeScanQuery({ anyOf: [{ names: { exact: ['Sensor'] } }] })
    expect(withoutAddresses.anyOf[0].addresses).toBeNull()
    expect(canonicalScanQueryJson(withoutAddresses.anyOf[0])).not.toContain('addresses')
  })

  test('rejects malformed, empty, and non-array addresses clauses', () => {
    expectCode(() => normalizeScanQuery({ anyOf: [{ addresses: [] }] }), 'scan.filter-invalid')
    expectCode(() => normalizeScanQuery({ anyOf: [{ addresses: 'aa:bb:cc:dd:ee:ff' }] }), 'scan.filter-invalid')
    expectCode(() => normalizeScanQuery({ anyOf: [{ addresses: ['not-an-address'] }] }), 'scan.filter-invalid')
    expectCode(() => normalizeScanQuery({ anyOf: [{ addresses: ['aabbccddeeff'] }] }), 'scan.filter-invalid')
    expectCode(() => normalizeScanQuery({ anyOf: [{ addresses: [42] }] }), 'scan.filter-invalid')
  })

  test('matches observations by address and treats address-less observations as non-matching', () => {
    const query = normalizeScanQuery({ anyOf: [{ addresses: ['98:75:96:A2:14:34'] }] })
    const matching = normalizeScanObservation(
      observation({ address: { type: 'public', value: '98:75:96:A2:14:34' } })
    )
    expect(matching.address).toEqual({ type: 'public', value: '98:75:96:A2:14:34' })
    expect(observationMatchesScanQuery(query, matching)).toBe(true)

    const differentAddress = normalizeScanObservation(
      observation({ address: { type: 'public', value: 'AA:BB:CC:DD:EE:FF' } })
    )
    expect(observationMatchesScanQuery(query, differentAddress)).toBe(false)

    // Platforms that expose no address cannot satisfy an addresses clause.
    const addressless = normalizeScanObservation(observation())
    expect(addressless.address).toBeUndefined()
    expect(observationMatchesScanQuery(query, addressless)).toBe(false)
  })

  test('applies addresses in exclusion clauses', () => {
    const query = normalizeScanQuery({
      anyOf: [{ names: { exact: ['Sensor'] } }],
      exclude: [{ addresses: ['98:75:96:A2:14:34'] }]
    })
    const excluded = normalizeScanObservation(
      observation({ address: { type: 'random', value: '98:75:96:A2:14:34' } })
    )
    expect(observationMatchesScanQuery(query, excluded)).toBe(false)
    const kept = normalizeScanObservation(observation({ address: { type: 'public', value: '11:22:33:44:55:66' } }))
    expect(observationMatchesScanQuery(query, kept)).toBe(true)
  })
})

describe('fail-closed behavior on a backend without peer:address-targeting', () => {
  test('scan with an addresses clause fails closed with capability.unsupported', async () => {
    const { manager } = await createDeterministicTestBleManager()
    try {
      await expect(
        manager.scan({ query: { anyOf: [{ addresses: ['98:75:96:A2:14:34'] }] } })
      ).rejects.toMatchObject({ code: 'capability.unsupported' })
    } finally {
      await manager.destroy()
    }
  })

  test('connect by address fails closed with capability.unsupported', async () => {
    const { manager } = await createDeterministicTestBleManager()
    try {
      await expect(manager.connect({ address: '98:75:96:A2:14:34' })).rejects.toMatchObject({
        code: 'capability.unsupported'
      })
      await expect(manager.connect({ address: 'aa-bb-cc-dd-ee-ff', addressType: 'random' })).rejects.toMatchObject({
        code: 'capability.unsupported'
      })
    } finally {
      await manager.destroy()
    }
  })

  test('malformed connect address targets are rejected before any capability decision', async () => {
    const { manager } = await createDeterministicTestBleManager()
    try {
      await expect(manager.connect({ address: 'not-an-address' })).rejects.toMatchObject({
        code: 'argument.invalid'
      })
      await expect(manager.connect({ address: '98:75:96:A2:14:34', addressType: 'opaque' })).rejects.toMatchObject({
        code: 'argument.invalid'
      })
    } finally {
      await manager.destroy()
    }
  })
})
