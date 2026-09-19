'use strict'

// D9: createDbusNextBluezBackendProvider is a historical alias for the shared
// Rust core provider (no dbus-next transport is used). The alias must resolve
// to createDesktopRustCoreBackendProvider: same descriptor, same binding use.

function withPlatform(platform, run) {
  const original = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: platform })
  try {
    return run()
  } finally {
    Object.defineProperty(process, 'platform', original)
  }
}

function stubBinding(seen) {
  return {
    openProduction: async () => {
      seen.push('openProduction')
      throw new Error('no hardware in alias probe')
    },
    openSynthetic: async () => {
      seen.push('openSynthetic')
      throw new Error('alias probe must not open the synthetic radio')
    },
    listAdapters: async () => {
      seen.push('listAdapters')
      return []
    },
    capabilityStates: () => {
      seen.push('capabilityStates')
      return []
    }
  }
}

describe('createDbusNextBluezBackendProvider historical alias', () => {
  test('resolves to the shared Rust core provider (same descriptor, same binding)', async () => {
    const { createDbusNextBluezBackendProvider } = require('../../../src/node-bluez')
    const {
      createDesktopRustCoreBackendProvider,
      DESKTOP_RUST_CORE_PROFILES
    } = require('../../../src/backends/desktop/desktop-rust-core-provider')
    const now = () => 7
    const aliasSeen = []
    const directSeen = []
    const alias = withPlatform('linux', () =>
      createDbusNextBluezBackendProvider({ busKind: 'system', now, owner: 'alias-probe', binding: stubBinding(aliasSeen) })
    )
    const direct = withPlatform('linux', () =>
      createDesktopRustCoreBackendProvider({
        platform: 'bluez',
        owner: 'alias-probe',
        now,
        hostKind: 'node',
        bluezBus: 'system',
        binding: stubBinding(directSeen)
      })
    )
    expect(alias.descriptor).toEqual(direct.descriptor)
    expect(alias.descriptor.providerId).toBe(DESKTOP_RUST_CORE_PROFILES.bluez.providerId)
    // The hardware-free stub lists no adapters, so both legs refuse identically
    // through the same binding surface — and neither opens the synthetic radio.
    await expect(withPlatform('linux', () => alias.listAdapters())).rejects.toMatchObject({
      normalized: { code: 'adapter.unavailable' }
    })
    await expect(withPlatform('linux', () => direct.listAdapters())).rejects.toMatchObject({
      normalized: { code: 'adapter.unavailable' }
    })
    expect(aliasSeen).toEqual(directSeen)
    expect(aliasSeen).toEqual(expect.arrayContaining(['listAdapters']))
    expect(aliasSeen).not.toContain('openSynthetic')
  })
})
