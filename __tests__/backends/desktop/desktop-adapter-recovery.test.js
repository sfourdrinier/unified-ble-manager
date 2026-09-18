// __tests__/backends/desktop/desktop-adapter-recovery.test.js
//
// Adapter off then on under a supervised connection on every desktop host,
// over the real N-API addon's synthetic radio, the legacy core manager and the
// public manager and supervisor. 5.0 keeps the manager, and the peer handle,
// through an adapter loss so the supervisor reconnects when the adapter
// returns (legacy destroyed the manager: `unified-ble-core.ts`
// `applyAdapterStateEvent` / `releaseResources('backend-restart')`), including
// an outage longer than the public 10 s readiness window.

const h = require('../../helpers/desktop-rust-core-harness')
const {
  createDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createConnectionSupervisor } = require('../../../src/public/connection-supervisor')

jest.setTimeout(30_000)

let describeState = () => null
const RETRY = Object.freeze({ initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 })

async function eventually(what, predicate, timeoutMs = 8000) {
  const started = Date.now()
  while (!predicate()) {
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for ${what}: ${JSON.stringify(describeState())}`)
    }
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

async function open(platform) {
  const clock = { offset: 0, now: () => performance.now() + clock.offset }
  const harness = h.realBinding(platform)
  const provider = createDesktopRustCoreBackendProvider({
    platform,
    owner: `recovery-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: h.HOST_PLATFORM[platform]
  })
  const internal = await createNodeBleManagerFromProvider(
    provider,
    DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
    {
      now: () => performance.now()
    }
  )
  const manager = await createPublicBleManager(internal, clock.now)
  const stage = harness.opened[harness.opened.length - 1]
  await stage.stageServices('peer-1', h.hrmServices())
  const scan = await internal.scan(h.scanOptions())
  const iterator = scan.observations[Symbol.asyncIterator]()
  await stage.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Polar H10' })
  const observation = await h.nextValue(iterator, 5000)
  await iterator.return?.()
  await scan.stop()
  return { clock, internal, manager, stage, peerId: String(observation.device.id) }
}

describe.each(['corebluetooth', 'winrt', 'bluez'])('%s: adapter off and on under a supervised connection', platform => {
  test('the supervisor reconnects when the adapter returns, after more than the readiness window', async () => {
    const { clock, manager, stage, peerId } = await open(platform)
    const supervisor = createConnectionSupervisor(manager, peerId, { retry: RETRY })
    supervisor.start()
    describeState = () => supervisor.snapshot
    await eventually('the first connection', () => supervisor.snapshot.state === 'connected')
    const first = supervisor.snapshot.connectionGeneration

    await stage.stageAdapterState('powered-off', true)
    await eventually('the loss', () => supervisor.snapshot.lastDisconnect?.cause === 'adapter-loss')
    if (platform !== 'bluez') {
      // CoreBluetooth and WinRT refuse a connect on a lost adapter before any
      // effect, so the supervisor waits for the adapter. BlueZ keeps its legacy
      // lifecycle-only admission: the OS answers the connect itself.
      await eventually('the adapter wait', () => supervisor.snapshot.state === 'waiting-for-gate')
      // The adapter stays off past the public readiness window (10 s).
      clock.offset += 11_000
      await stage.stageAdapterState('powered-off', true)
      await new Promise(resolve => setTimeout(resolve, 100))
      expect(supervisor.snapshot.state).toBe('waiting-for-gate')
    }

    await stage.stageAdapterState('powered-on', true)
    await eventually('the reconnection', () => supervisor.snapshot.state === 'connected')
    expect(supervisor.snapshot.connectionGeneration).not.toBe(first)

    // The manager follows the new generation: a later link loss on the new
    // connection is observed and the supervisor reconnects again.
    const recovered = supervisor.snapshot.connectionGeneration
    await stage.stageLinkLoss('peer-1')
    await eventually(
      'the second reconnection',
      () => supervisor.snapshot.state === 'connected' && supervisor.snapshot.connectionGeneration !== recovered
    )
    expect((await supervisor.stop()).state).toBe('released')
    expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
  })
})
