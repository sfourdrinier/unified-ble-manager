'use strict'

// D5 (finding 100, owner decision L): test seams live in /testing, never in
// production entrypoints. Production option keys exclude radio/loadBinding/
// hostPlatform/firstStateTimeoutMs; DESKTOP_RUST_CORE_PARITY and the
// seam-accepting factory are exported from unified-ble-manager/testing only;
// the production factory refuses seam keys fail-closed and never opens the
// synthetic radio.

const PRODUCTION_ENTRYPOINTS = ['node-bluez', 'node-corebluetooth', 'node-winrt', 'electron-main']

function loadProduction(module) {
  return require(`../../../src/${module}`)
}

function loadTesting() {
  return require('../../../src/testing')
}

const SEAM_KEYS = ['radio', 'loadBinding', 'hostPlatform', 'firstStateTimeoutMs']

describe('production entrypoints exclude desktop test seams', () => {
  test.each(PRODUCTION_ENTRYPOINTS)('%s exports no seam surface', module => {
    const exported = loadProduction(module)
    expect(exported.DESKTOP_RUST_CORE_PARITY).toBeUndefined()
    expect(exported.createTestDesktopRustCoreBackendProvider).toBeUndefined()
    expect(exported.openSynthetic).toBeUndefined()
  })

  test('unified-ble-manager/testing exports the relocated seams', () => {
    const testing = loadTesting()
    expect(typeof testing.createTestDesktopRustCoreBackendProvider).toBe('function')
    expect(Array.isArray(testing.DESKTOP_RUST_CORE_PARITY)).toBe(true)
    expect(testing.DESKTOP_RUST_CORE_PARITY.length).toBeGreaterThan(0)
  })

  test.each(SEAM_KEYS)('production factory refuses the %s seam key fail-closed', key => {
    const { createDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')
    const calls = []
    const binding = {
      openProduction: async () => {
        calls.push('openProduction')
        throw new Error('no hardware in seam probe')
      },
      openSynthetic: async () => {
        calls.push('openSynthetic')
        throw new Error('production must never open the synthetic radio')
      },
      listAdapters: async () => {
        calls.push('listAdapters')
        return []
      },
      capabilityStates: () => {
        calls.push('capabilityStates')
        return []
      }
    }
    const seamValue =
      key === 'radio'
        ? 'synthetic'
        : key === 'hostPlatform'
          ? 'darwin'
          : key === 'firstStateTimeoutMs'
            ? 1
            : async () => binding
    expect(() =>
      createDesktopRustCoreBackendProvider({
        platform: 'corebluetooth',
        owner: 'seam-probe',
        now: () => 0,
        binding,
        [key]: seamValue
      })
    ).toThrow(expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) }))
    expect(calls).toEqual([])
  })

  test('testing factory still accepts the relocated seams', () => {
    const {
      createTestDesktopRustCoreBackendProvider
    } = require('../../../src/backends/desktop/desktop-rust-core-provider')
    const binding = {
      openProduction: async () => {
        throw new Error('no hardware in seam probe')
      },
      openSynthetic: async () => {
        throw new Error('not opened in this probe')
      },
      listAdapters: async () => [],
      capabilityStates: () => []
    }
    const provider = createTestDesktopRustCoreBackendProvider({
      platform: 'corebluetooth',
      owner: 'seam-probe',
      now: () => 0,
      radio: 'production',
      hostPlatform: 'darwin',
      firstStateTimeoutMs: 1,
      loadBinding: async () => binding,
      binding
    })
    expect(provider.descriptor.providerId).toBe('unified-ble:corebluetooth-provider')
  })
})
