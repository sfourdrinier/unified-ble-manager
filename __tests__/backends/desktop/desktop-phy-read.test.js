const {
  DesktopRustCoreBackend,
  DESKTOP_RUST_CORE_PROFILES,
  desktopRustCoreWiring,
  createDesktopRustCoreFeatureRegistry
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { BUILT_IN_FEATURE_IDS } = require('../../../src/backend-contract/capabilities')

function fixture(answer) {
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  const readPhy = jest.fn(async () => answer)
  Object.assign(backend, {
    op: operation => operation,
    assertOperational: () => {},
    liveConnection: () => ({ nativePeerId: 'native-peer', lease: 'native-lease' }),
    withTicket: (_correlation, _signal, _operation, action) => action('ticket'),
    budget: () => ({ timeoutMs: 700 }),
    now: () => 42,
    succeededTerminal: correlation => ({ correlation }),
    dispatchFor: (_correlation, completion) => ({ completion }),
    central: { readPhy }
  })
  return { backend, readPhy }
}

test.each([
  ['le-1m', 'le-2m'],
  ['le-2m', 'le-coded'],
  ['le-coded', 'le-1m']
])('desktop PHY observes separate TX %s and RX %s through native dispatch', async (txPhy, rxPhy) => {
  const { backend, readPhy } = fixture({ txPhy, rxPhy })
  const dispatch = backend.readPhy({}, { operation: { correlation: 'read-phy', signal: null, deadline: null } })
  await expect(dispatch.completion).resolves.toMatchObject({ txPhy, rxPhy, observedAtMonotonicMs: 42 })
  expect(readPhy).toHaveBeenCalledWith({
    peerId: 'native-peer',
    lease: 'native-lease',
    ticket: 'ticket',
    timeoutMs: 700
  })
})

test.each([null, 'unknown', 'le-4m'])('desktop PHY rejects malformed native observation %s', async txPhy => {
  const { backend } = fixture({ txPhy, rxPhy: 'le-1m' })
  await expect(
    backend.readPhy({}, { operation: { correlation: 'read-phy', signal: null, deadline: null } }).completion
  ).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
})

test('WinRT PHY capability declares observation-only and retains runtime refusal', () => {
  const wiring = desktopRustCoreWiring([
    { id: BUILT_IN_FEATURE_IDS.connectionPhy, state: 'limited', limitation: 'winrt-phy-observation-only' }
  ])
  expect(wiring.phy).toBe(true)
  const registry = createDesktopRustCoreFeatureRegistry(DESKTOP_RUST_CORE_PROFILES.winrt, wiring)
  expect(registry.registrations.find(row => row.id === BUILT_IN_FEATURE_IDS.connectionPhy)).toMatchObject({
    state: 'limited',
    limitations: [{ code: 'winrt-phy-observation-only' }]
  })
  const absent = createDesktopRustCoreFeatureRegistry(
    DESKTOP_RUST_CORE_PROFILES.winrt,
    desktopRustCoreWiring([
      {
        id: BUILT_IN_FEATURE_IDS.connectionPhy,
        state: 'unavailable',
        limitation: 'winrt-connection-phy-requires-windows-11-22000'
      }
    ])
  )
  expect(absent.registrations.find(row => row.id === BUILT_IN_FEATURE_IDS.connectionPhy)).toMatchObject({
    state: 'unavailable',
    limitations: [{ code: 'winrt-connection-phy-requires-windows-11-22000' }]
  })
})
