'use strict'

// Public facade -> desktop provider -> real addon -> Rust dispatch/central.
// Native answers are staged on the explicit synthetic radio, not hardware.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { createWinRtBleManager } = require('../../../src/node-winrt')

test('public parameter read and watch reach the inner NAPI radio', async () => {
  const harness = h.realBinding('winrt')
  const now = () => performance.now()
  const manager =
    process.platform === 'win32'
      ? await createWinRtBleManager({ binding: harness.binding, now })
      : await createPublicBleManager(
          await createNodeBleManagerFromProvider(
            createTestDesktopRustCoreBackendProvider({
              platform: 'winrt',
              owner: 'parameter-route',
              now,
              radio: 'synthetic',
              binding: harness.binding,
              hostPlatform: 'win32'
            }),
            DESKTOP_RUST_CORE_PROFILES.winrt.compatibility,
            { now }
          ),
          now
        )
  const stage = harness.opened.at(-1)
  let scan, connection, events
  try {
    scan = await manager.scan()
    const observations = scan.observations[Symbol.asyncIterator]()
    const pendingPeer = h.nextValue(observations, 5000)
    await stage.stageAdvertisement({ peerId: 'parameter-peer', localName: 'Parameters' })
    const peer = (await pendingPeer).peer
    await observations.return?.()
    await scan.stop()
    connection = await manager.connect(peer)
    await stage.stageConnectionPhy('parameter-peer', 'le-2m', 'le-coded')
    await expect(connection.controls.readPhy()).resolves.toMatchObject({
      state: 'measured',
      tx: 'le-2m',
      rx: 'le-coded'
    })
    expect(harness.calls.some(([method]) => method === 'readPhy')).toBe(true)
    await expect(connection.controls.requestPhy({ tx: 'le-1m' })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    for (const priority of ['balanced', 'low-power', 'high-throughput']) {
      await expect(connection.controls.requestPriority(priority)).resolves.toMatchObject({
        state: 'accepted',
        requested: priority
      })
    }
    const priorityCalls = harness.calls.filter(([method]) => method === 'requestPriority')
    expect(priorityCalls).toHaveLength(3)
    await expect(connection.controls.requestPriority('invalid')).rejects.toMatchObject({ code: 'argument.invalid' })
    expect(harness.calls.filter(([method]) => method === 'requestPriority')).toHaveLength(3)
    await stage.stageConnectionParameters('parameter-peer', 30_000, 2, 4_000_000, false)
    await expect(connection.controls.parameters()).resolves.toMatchObject({
      state: 'measured',
      intervalMs: 30,
      peripheralLatency: 2,
      supervisionTimeoutMs: 4000
    })
    events = connection.controls.parameterEvents()[Symbol.asyncIterator]()
    await expect(events.next()).resolves.toMatchObject({ done: false, value: { intervalMs: 30 } })
    await stage.stageConnectionParameters('parameter-peer', 60_000, 3, 5_000_000, true)
    await expect(events.next()).resolves.toMatchObject({
      done: false,
      value: {
        intervalMs: 60,
        peripheralLatency: 3,
        supervisionTimeoutMs: 5000
      }
    })
    expect(harness.calls.filter(([method]) => method === 'connectionParameters').length).toBeGreaterThanOrEqual(2)
    expect(
      harness.calls.some(([method, args]) => method === 'connectionParameters' && args[0].observationWatch === true)
    ).toBe(true)
    const reconciled = events.next()
    await stage.stageConnectionParameters('parameter-peer', 90_000, 4, 6_000_000, false)
    const beforeGap = harness.calls.filter(([method]) => method === 'connectionParameters').length
    await stage.stageConnectionParameterGap('parameter-peer', 3)
    await expect(reconciled).resolves.toMatchObject({ done: false, value: { intervalMs: 90 } })
    expect(harness.calls.filter(([method]) => method === 'connectionParameters').length).toBeGreaterThan(beforeGap)
    const failedRead = events.next()
    await stage.stageConnectionParameterFailure('parameter-peer', '0x80070005')
    await expect(failedRead).rejects.toMatchObject({
      code: 'platform.failure',
      operation: 'winrt.connection.parameters.event',
      platform: { domain: 'winrt', code: 'hresult', metadata: { hresult: '0x80070005' } }
    })
  } finally {
    await events?.return?.()
    await connection?.release()
    await scan?.stop()
    await manager.destroy()
  }
}, 30000)
