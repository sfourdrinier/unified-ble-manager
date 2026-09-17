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

  test('keeps the root import graph neutral and loads dbus-next only through the strict Node subpath', () => {
    jest.isolateModules(() => {
      const root = require('../../../src')
      // PR1: root is application-only, no generic BleManager class. Advanced holds the low-level one.
      expect(root.ApplicationBleManager).toBeUndefined()
      expect(root.createPublicBleManager).toBeUndefined()
      expect(root.BleManager).toBeUndefined()
      expect(mockDbusLoads).toBe(0)

      const bluez = require('../../../src/node-bluez')
      expect(typeof bluez.createDbusNextBluezBackendProvider).toBe('function')
      expect(mockDbusLoads).toBe(1)
    })
  })

  // R03 cutover (surfaced contract change): the live provider executes the
  // shared Rust core, not the TypeScript D-Bus transport. The factory keeps
  // its name and bus selection for compatibility, but BLE work routes
  // through `UbmCentral`; a missing core fails loudly instead of silently
  // constructing TS execution. The `DbusNext` boundary stays exported for
  // package-surface compatibility only.
  test('creates the live provider over the shared core with an explicit bus kind', () => {
    const { createDbusNextBluezBackendProvider, DbusNextBluezBoundaryFactory } = require('../../../src/node-bluez')
    expect(typeof DbusNextBluezBoundaryFactory).toBe('function')
    const binding = {
      openProduction: async () => {
        throw new Error('no hardware in this surface probe')
      },
      openSynthetic: async () => {
        throw new Error('production surface probe must not open the synthetic radio')
      }
    }
    const provider = createDbusNextBluezBackendProvider({ busKind: 'session', now: () => 10, binding })

    expect(provider.descriptor).toMatchObject({
      hostKind: 'node',
      loadability: 'loadable',
      providerId: 'unified-ble:bluez-rust-core-provider'
    })
  })

  test('live provider without a core fails loudly instead of building TS execution', () => {
    const previous = process.env.UBM_NAPI_ADDON
    process.env.UBM_NAPI_ADDON = require('node:path').join(__dirname, 'fixtures', 'missing-addon.node')
    try {
      const { createDbusNextBluezBackendProvider } = require('../../../src/node-bluez')
      expect(() => createDbusNextBluezBackendProvider({ busKind: 'session', now: () => 10 })).toThrow(
        expect.objectContaining({
          normalized: expect.objectContaining({
            code: 'capability.unsupported',
            operation: 'bluez-manager.rust-core-missing'
          })
        })
      )
    } finally {
      if (previous === undefined) delete process.env.UBM_NAPI_ADDON
      else process.env.UBM_NAPI_ADDON = previous
    }
  })
})
