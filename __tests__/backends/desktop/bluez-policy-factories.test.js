'use strict'

jest.mock('../../../src/node-desktop-manager', () => ({
  admitBluezBusKind: value => value,
  createDesktopCoreBleManager: jest.fn(() => Promise.resolve('manager')),
  createDesktopCoreProcessHost: jest.fn(() => Promise.resolve('process')),
  createDesktopCoreProvider: jest.fn(() => 'provider')
}))
const factories = require('../../../src/node-desktop-manager')
const bluez = require('../../../src/node-bluez')
const electron = require('../../../src/electron-main')

beforeEach(() => jest.clearAllMocks())
test.each([
  ['manager', options => bluez.createBluezBleManager(options), 'createDesktopCoreBleManager', 2],
  ['process owner', options => bluez.createBluezProcessHost(options), 'createDesktopCoreProcessHost', 2],
  ['Node provider', options => bluez.createDbusNextBluezBackendProvider(options), 'createDesktopCoreProvider', 3],
  [
    'Electron provider',
    options => electron.createElectronMainBluezBackendProvider(options),
    'createDesktopCoreProvider',
    3
  ]
])(
  '%s forwards one immutable LE policy, with no compatibility fallback',
  async (_name, create, factory, extrasIndex) => {
    const policy = { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }
    await create({ now: () => 0, busKind: 'session', connectionPolicy: policy })
    const call = factories[factory].mock.calls[0]
    expect(call[extrasIndex].connectionPolicy).toEqual(policy)
    expect(call[extrasIndex].connectionPolicy).not.toBe(policy)
    expect(Object.isFrozen(call[extrasIndex].connectionPolicy)).toBe(true)
    expect(call[1]).not.toHaveProperty('connectionPolicy')
    factories[factory].mockClear()
    await expect(
      Promise.resolve().then(() =>
        create({ now: () => 0, busKind: 'system', connectionPolicy: { mode: 'legacy-device-wide' } })
      )
    ).rejects.toThrow('argument.invalid')
    expect(factories[factory]).not.toHaveBeenCalled()
  }
)
