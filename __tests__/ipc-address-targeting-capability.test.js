// __tests__/ipc-address-targeting-capability.test.js
// IPC preserves the instantiated authority's address-targeting capability.
const { IpcPublicManagerAdapter } = require('../src/ipc/public-manager')

function descriptor(id, state) {
  return {
    id,
    state,
    selectedSchemaRange: { minimum: 1, maximum: 1 },
    implementationOrigin: 'backend-native',
    tck: { status: 'not-run' },
    evidence: { level: 'none' },
    limitations: [],
    limits: {}
  }
}

/** Stands in for a main-process snapshot that does advertise the capability. */
function capabilitiesAdvertisingAddressTargeting() {
  const all = [descriptor('peer:address-targeting', 'supported'), descriptor('connection:direct', 'supported')]
  return {
    supports: id => all.some(entry => entry.id === id && entry.state === 'supported'),
    get: id => all.find(entry => entry.id === id),
    require: id => {
      const found = all.find(entry => entry.id === id)
      if (!found) throw new Error(`missing ${id}`)
      return found
    },
    list: () => all
  }
}

function ipcManagerWith(
  capabilities,
  connect = async () => {
    throw new Error('native refusal')
  },
  scan = async () => {
    throw new Error('native refusal')
  }
) {
  const ipc = {
    capabilities,
    bootstrap: { discovery: { kind: 'scan' } },
    connect,
    scan
  }
  return new IpcPublicManagerAdapter(ipc, {
    capabilities,
    adapter: { state: async () => ({ availability: 'available', authorization: 'granted', power: 'on' }) },
    discoveryKind: 'scan'
  })
}

describe('IPC address-targeting capability honesty', () => {
  test('retains the capability advertised by the native authority', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    expect(manager.capabilities.get('peer:address-targeting').state).toBe('supported')
    expect(manager.capabilities.supports('peer:address-targeting')).toBe(true)
  })

  test('list() agrees with get(), so enumeration cannot disagree with a lookup', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    const listed = manager.capabilities.list().find(entry => entry.id === 'peer:address-targeting')
    expect(listed.state).toBe('supported')
  })

  test('leaves every other capability untouched', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    expect(manager.capabilities.get('connection:direct').state).toBe('supported')
    expect(manager.capabilities.supports('connection:direct')).toBe(true)
  })

  test('connecting to an address reaches IPC without scanning and retains default public address type', async () => {
    const connect = jest.fn(async () => {
      throw new Error('native refusal')
    })
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting(), connect)
    await expect(manager.connect({ address: '98:75:96:A2:14:34' })).rejects.toThrow('native refusal')
    expect(connect.mock.calls[0][0]).toEqual({ address: '98:75:96:A2:14:34', addressType: 'public' })
  })

  test('native unsupported capability remains unsupported and cannot dispatch', async () => {
    const capabilities = capabilitiesAdvertisingAddressTargeting()
    capabilities.get('peer:address-targeting').state = 'unsupported'
    const connect = jest.fn()
    const manager = ipcManagerWith(capabilities, connect)
    await expect(manager.connect({ address: '98:75:96:A2:14:34' })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    expect(connect).not.toHaveBeenCalled()
  })

  test.each([
    ['public', 'public'],
    ['random', 'random'],
    [null, 'opaque'],
    [undefined, 'opaque']
  ])(
    'address-filtered scan preserves native type %s in a matching compact observation',
    async (addressType, expectedType) => {
      const { CoreBoundedStream } = require('../src/core/bounded-stream')
      const { capacity } = require('../src/backend-contract/primitives')
      const observations = new CoreBoundedStream(
        { itemCapacity: capacity(8), byteCapacity: capacity(8192), reservedControlCapacity: capacity(1) },
        'error'
      )
      const scan = jest.fn(async () => ({ plan: null, observations, stop: async () => observations.close() }))
      const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting(), undefined, scan)
      const session = await manager.scan({
        query: { anyOf: [{ addresses: ['aa:bb:cc:dd:ee:ff'] }] },
        duplicates: 'all'
      })
      expect(scan.mock.calls[0][0].query.anyOf[0].addresses).toEqual(['AA:BB:CC:DD:EE:FF'])
      const next = session.observations[Symbol.asyncIterator]().next()
      observations.emit(
        {
          peerId: 'other',
          address: '11:22:33:44:55:66',
          localName: null,
          rssi: -40,
          txPowerLevel: null,
          serviceUuids: [],
          manufacturerData: [],
          serviceData: []
        },
        1
      )
      observations.emit(
        {
          peerId: 'target',
          address: 'AA:BB:CC:DD:EE:FF',
          addressType,
          localName: null,
          rssi: -40,
          txPowerLevel: null,
          serviceUuids: [],
          manufacturerData: [],
          serviceData: []
        },
        1
      )
      await expect(next).resolves.toMatchObject({
        value: {
          kind: 'value',
          value: { address: { value: 'AA:BB:CC:DD:EE:FF', type: expectedType }, peer: { id: 'target' } }
        }
      })
      await session.stop()
    }
  )
})
