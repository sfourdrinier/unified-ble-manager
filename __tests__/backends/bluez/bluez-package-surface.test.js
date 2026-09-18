// __tests__/backends/bluez/bluez-package-surface.test.js

let mockDbusLoads = 0

jest.mock('dbus-next', () => {
  mockDbusLoads += 1
  return {
    MessageType: { SIGNAL: 4 },
    Variant: class Variant {},
    systemBus: jest.fn(),
    sessionBus: jest.fn()
  }
})

describe('BlueZ package surface', () => {
  beforeEach(() => {
    mockDbusLoads = 0
    jest.resetModules()
  })

  function onLinux(run) {
    const original = Object.getOwnPropertyDescriptor(process, 'platform')
    Object.defineProperty(process, 'platform', { configurable: true, value: 'linux' })
    try {
      return run()
    } finally {
      Object.defineProperty(process, 'platform', original)
    }
  }

  test('keeps the root import graph neutral; node/bluez runs the Rust core and never loads dbus-next', () => {
    jest.isolateModules(() => {
      const root = require('../../../src')
      // PR1: root is application-only, no generic BleManager class. Advanced holds the low-level one.
      expect(root.ApplicationBleManager).toBeUndefined()
      expect(root.createPublicBleManager).toBeUndefined()
      expect(root.BleManager).toBeUndefined()
      expect(mockDbusLoads).toBe(0)

      const bluez = require('../../../src/node-bluez')
      expect(typeof bluez.createDbusNextBluezBackendProvider).toBe('function')
      // PR210-02: the D-Bus transport is legacy and unreachable from the
      // public entrypoint; BLE work executes the shared Rust core.
      expect(bluez.DbusNextBluezBoundaryFactory).toBeUndefined()
      expect(mockDbusLoads).toBe(0)
    })
  })

  test('creates the live provider over the shared core on the system bus', () => {
    const { createDbusNextBluezBackendProvider } = require('../../../src/node-bluez')
    const binding = {
      openProduction: async () => {
        throw new Error('no hardware in this surface probe')
      },
      openSynthetic: async () => {
        throw new Error('production surface probe must not open the synthetic radio')
      },
      listAdapters: async () => []
    }
    const provider = onLinux(() => createDbusNextBluezBackendProvider({ busKind: 'system', now: () => 10, binding }))
    expect(provider.descriptor).toMatchObject({
      hostKind: 'node',
      loadability: 'loadable',
      providerId: 'unified-ble:bluez-dbus-provider'
    })
  })

  test('a missing core fails loudly on first use instead of building TS execution', async () => {
    const { createDbusNextBluezBackendProvider } = require('../../../src/node-bluez')
    const provider = onLinux(() => createDbusNextBluezBackendProvider({ busKind: 'system', now: () => 10 }))
    await expect(provider.listAdapters()).rejects.toMatchObject({
      normalized: { code: 'capability.unavailable', operation: 'bluez.native-boundary.load' }
    })
    expect(mockDbusLoads).toBe(0)
  })
})
