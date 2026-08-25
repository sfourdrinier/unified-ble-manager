// __tests__/ipc-address-targeting-capability.test.js
// The renderer inherits the main process's capability snapshot, so a BlueZ
// backend advertising peer:address-targeting would otherwise make the IPC
// surface claim a feature its versioned schema cannot carry — every call then
// failing. Fail-closed means the capability must read as unsupported there.
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
  const all = [
    descriptor('peer:address-targeting', 'supported'),
    descriptor('connection:direct', 'supported')
  ]
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

function ipcManagerWith(capabilities) {
  const ipc = {
    capabilities,
    bootstrap: { discovery: { kind: 'scan' } },
    connect: async () => {
      throw new Error('the transport should never be reached for an address target')
    }
  }
  return new IpcPublicManagerAdapter(ipc, {
    capabilities,
    adapter: { state: async () => ({ availability: 'available', authorization: 'granted', power: 'on' }) },
    discoveryKind: 'scan'
  })
}

describe('IPC address-targeting capability honesty', () => {
  test('reports the capability as unsupported even when the host advertises it', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    expect(manager.capabilities.get('peer:address-targeting').state).toBe('unsupported')
    expect(manager.capabilities.supports('peer:address-targeting')).toBe(false)
  })

  test('list() agrees with get(), so enumeration cannot disagree with a lookup', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    const listed = manager.capabilities.list().find(entry => entry.id === 'peer:address-targeting')
    expect(listed.state).toBe('unsupported')
  })

  test('leaves every other capability untouched', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    expect(manager.capabilities.get('connection:direct').state).toBe('supported')
    expect(manager.capabilities.supports('connection:direct')).toBe(true)
  })

  test('connecting to an address fails closed rather than reaching the transport', async () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingAddressTargeting())
    await expect(manager.connect({ address: '98:75:96:A2:14:34' })).rejects.toThrow(
      /capability\.unsupported: ipc-public-manager\.connect\.address/
    )
  })
})
