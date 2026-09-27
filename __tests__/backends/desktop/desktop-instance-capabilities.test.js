'use strict'
const { decodeDesktopRustCoreCapabilityStates } = require('../../../src/backends/desktop/desktop-rust-core-binding')
const { createTestDesktopRustCoreBackendProvider } = require('../../../src/backends/desktop/desktop-rust-core-provider')

test('public desktop feature registrations preserve instance connection refusal and reason', () => {
  const {
    createDesktopRustCoreFeatureRegistry,
    desktopRustCoreWiring,
    DESKTOP_RUST_CORE_PROFILES
  } = require('../../../src/backends/desktop/desktop-rust-core-provider')
  const states = ['connection:direct', 'background:desktop-maintain-connection'].map(id => ({
    id,
    state: 'unsupported',
    limitation: 'bluez-le-bearer-attestation-required'
  }))
  const registry = createDesktopRustCoreFeatureRegistry(DESKTOP_RUST_CORE_PROFILES.bluez, desktopRustCoreWiring(states))
  for (const { id } of states) {
    expect(registry.registrations.find(row => row.id === id)).toMatchObject({
      state: 'unsupported',
      limitations: [{ code: 'bluez-le-bearer-attestation-required' }],
      evidence: { evidenceLevel: 'blocked' }
    })
  }
})

test('instance capability rows preserve reasons and reject malformed or sparse snapshots', () => {
  const input = [{ id: 'connection:direct', state: 'unsupported', limitation: 'bluez-le-bearer-attestation-required' }]
  expect(decodeDesktopRustCoreCapabilityStates(input, 'test.capabilities')).toEqual(input)
  for (const invalid of [
    null,
    {},
    new Array(1),
    [undefined],
    [{ id: '', state: 'limited' }],
    [{ id: 'x', state: 'unknown' }],
    [...input, ...input],
    [{ id: 'x', state: 'limited', limitation: 7 }]
  ]) {
    expect(() => decodeDesktopRustCoreCapabilityStates(invalid, 'test.capabilities')).toThrow('protocol.malformed')
  }
})

test.each(['supported', 'limited', 'unavailable', 'unsupported'])(
  'valid native %s state survives decoding and controls public admission',
  state => {
    const {
      createDesktopRustCoreFeatureRegistry,
      desktopRustCoreWiring,
      DESKTOP_RUST_CORE_PROFILES
    } = require('../../../src/backends/desktop/desktop-rust-core-provider')
    const rows = ['connection:direct', 'background:desktop-maintain-connection'].map(id => ({
      id,
      state,
      limitation: 'instance-reason'
    }))
    const decoded = decodeDesktopRustCoreCapabilityStates(rows, 'test.instance')
    expect(decoded).toEqual(rows)
    const wiring = desktopRustCoreWiring(decoded)
    expect(wiring.connectionDirect).toBe(state === 'supported' || state === 'limited')
    expect(wiring.maintainConnection).toBe(state === 'supported' || state === 'limited')
    const registry = createDesktopRustCoreFeatureRegistry(DESKTOP_RUST_CORE_PROFILES.winrt, wiring)
    for (const { id } of rows) {
      const projected = registry.registrations.find(row => row.id === id)
      if (state === 'unsupported' || state === 'unavailable') {
        expect(projected).toMatchObject({ state, limitations: [{ code: 'instance-reason' }] })
      } else {
        // Native support admits the mechanism; it does not promote deterministic evidence.
        expect(projected).toMatchObject({ state: 'limited' })
      }
    }
  }
)

test('provider reads the allocated instance, not a static table, and compensates failed projection', async () => {
  const runtimeCapabilityStates = jest.fn(async () => {
    throw new Error('instance projection refused')
  })
  const close = jest.fn(async () => ({ state: 'released', failures: [] }))
  const capabilityStates = jest.fn(() => {
    throw new Error('static table must not be read')
  })
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'bluez',
    hostPlatform: 'linux',
    owner: 'instance-test',
    now: () => 0,
    binding: {
      listAdapters: async () => [{ index: 0, label: 'hci0' }],
      openProduction: async () => ({ runtimeCapabilityStates, close }),
      capabilityStates
    }
  })
  await provider.listAdapters()
  expect(runtimeCapabilityStates).toHaveBeenCalledTimes(1)
  expect(capabilityStates).not.toHaveBeenCalled()
  expect(close).toHaveBeenCalledTimes(1)
})
