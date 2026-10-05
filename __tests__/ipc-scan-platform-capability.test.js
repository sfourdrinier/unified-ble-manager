// __tests__/ipc-scan-platform-capability.test.js
// IPC preserves supported platform scan options and native limitations.
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

function ipcManagerWith(
  capabilities,
  scan = async () => {
    throw new Error('native refusal')
  }
) {
  const ipc = {
    capabilities,
    bootstrap: { discovery: { kind: 'scan' } },
    scan
  }
  return new IpcPublicManagerAdapter(ipc, {
    capabilities,
    adapter: { state: async () => ({ availability: 'available', authorization: 'granted', power: 'on' }) },
    discoveryKind: 'scan'
  })
}

describe('IPC scan:platform-options capability honesty', () => {
  test('retains capability support advertised by the native authority', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    expect(manager.capabilities.get('scan:platform-options').state).toBe('supported')
    expect(manager.capabilities.supports('scan:platform-options')).toBe(true)
    expect(manager.capabilities.require('scan:platform-options').state).toBe('supported')
  })

  test('list() agrees with get(), so enumeration cannot disagree with a lookup', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    const listed = manager.capabilities.list().find(entry => entry.id === 'scan:platform-options')
    expect(listed.state).toBe('supported')
  })

  test('leaves every other capability untouched', () => {
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions())
    expect(manager.capabilities.get('connection:direct').state).toBe('supported')
    expect(manager.capabilities.supports('connection:direct')).toBe(true)
  })

  test('supported scan options reach IPC unchanged', async () => {
    const scan = jest.fn(async () => {
      throw new Error('native refusal')
    })
    const manager = ipcManagerWith(capabilitiesAdvertisingScanPlatformOptions(), scan)
    const platform = { kind: 'android', mode: 'low-power', reportDelayMs: 20, legacy: false, phy: 'coded' }
    await expect(manager.scan({ platform })).rejects.toThrow('native refusal')
    expect(scan.mock.calls[0][0].platform).toEqual(platform)
  })

  test('native unsupported platform options fail before IPC admission', async () => {
    const capabilities = capabilitiesAdvertisingScanPlatformOptions()
    capabilities.get('scan:platform-options').state = 'unsupported'
    const scan = jest.fn()
    const manager = ipcManagerWith(capabilities, scan)
    await expect(manager.scan({ platform: { kind: 'android' } })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    expect(scan).not.toHaveBeenCalled()
  })
})
