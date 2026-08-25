// __tests__/ipc-scan-platform-capability.test.js
// The renderer inherits the main process's capability snapshot, so a backend
// advertising scan:platform-options would otherwise make the IPC surface claim
// a feature its versioned scan request cannot carry — every scan carrying
// options.platform then failing. Fail-closed means the capability must read as
// unsupported there, and the scan itself must be rejected before IPC.
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
function capabilitiesAdvertisingScanPlatformOptions() {
  const all = [descriptor('scan:platform-options', 'supported'), descriptor('connection:direct', 'supported')]
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
    scan: async () => {
      throw new Error('the transport should never be reached for platform scan options')
    }
  }
  return new IpcPublicManagerAdapter(ipc, {
    capabilities,
    adapter: { state: async () => ({ availability: 'available', authorization: 'granted', power: 'on' }) },
    discoveryKind: 'scan'
  })
}

describe('IPC scan:platform-options capability honesty', () => {
  test('reports the capability as unsupported even when the host advertises it', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    expect(manager.capabilities.get('scan:platform-options').state).toBe('unsupported')
    expect(manager.capabilities.supports('scan:platform-options')).toBe(false)
    expect(manager.capabilities.require('scan:platform-options').state).toBe('unsupported')
  })

  test('list() agrees with get(), so enumeration cannot disagree with a lookup', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    const listed = manager.capabilities.list().find(entry => entry.id === 'scan:platform-options')
    expect(listed.state).toBe('unsupported')
  })

  test('leaves every other capability untouched', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    expect(manager.capabilities.get('connection:direct').state).toBe('supported')
    expect(manager.capabilities.supports('connection:direct')).toBe(true)
  })

  test('scanning with platform options fails closed rather than reaching the transport', async () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    await expect(manager.scan({ platform: { kind: 'android', mode: 'low-power' } })).rejects.toThrow(
      /capability\.unsupported: ipc-public-manager\.scan\.platform-options/
    )
  })
})
