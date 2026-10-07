'use strict'

// Public/provider/addon/native-central integration with an explicit synthetic
// CoreBluetooth profile. This is native integration evidence, not radio proof.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

test('public readiness writes retain owned bytes and native order until the radio is ready', async () => {
  const harness = h.realBinding('corebluetooth')
  const now = () => performance.now()
  const manager = await createPublicBleManager(
    await createNodeBleManagerFromProvider(
      createTestDesktopRustCoreBackendProvider({
        platform: 'corebluetooth',
        owner: 'ready-public-route',
        now,
        radio: 'synthetic',
        binding: harness.binding,
        hostPlatform: 'darwin'
      }),
      DESKTOP_RUST_CORE_PROFILES.corebluetooth.compatibility,
      { now }
    ),
    now
  )
  const stage = harness.opened.at(-1)
  let scan, connection
  try {
    scan = await manager.scan()
    const observations = scan.observations[Symbol.asyncIterator]()
    const observed = h.nextValue(observations, 5000)
    await stage.stageAdvertisement({ peerId: 'ready-peer', localName: 'Ready' })
    const peer = (await observed).peer
    await observations.return?.()
    await scan.stop()
    connection = await manager.connect(peer)
    await stage.stageServices('ready-peer', h.hrmServices())
    await stage.stageMtu('ready-peer', 23)
    await stage.stageWriteLimits('ready-peer', 20, 20)
    await stage.stageWriteReadiness('ready-peer', false, false)
    const database = await connection.discover()
    const characteristic = database.characteristic('180d', '2a37')
    const input = new Uint8Array([42])
    const first = characteristic.writeWhenReady(input)
    input[0] = 99
    const second = characteristic.write(new Uint8Array([2]), { response: 'not-required' })
    let stopped = false
    const nativeProbe = (async () => {
      while (!stopped) {
        if ((await stage.stagedRadioCalls()).includes('write_without_response_ready')) return
        await new Promise(resolve => setImmediate(resolve))
      }
    })()
    try {
      await h.withTimeout(nativeProbe, 5000, 'owned native readiness probe')
    } finally {
      stopped = true
    }
    expect(harness.calls.filter(([method]) => method === 'writeReadiness')).toHaveLength(0)
    expect(await stage.stagedWriteValues()).toEqual([])
    await stage.stageWriteReadiness('ready-peer', true, true)
    await expect(first).resolves.toMatchObject({ commitState: 'unknown' })
    await expect(second).resolves.toMatchObject({ commitState: 'unknown' })
    expect((await stage.stagedWriteValues()).map(value => [...value])).toEqual([[42], [2]])
    expect(harness.calls.filter(([method]) => method === 'writeWhenReady')).toHaveLength(1)
  } finally {
    await connection?.release()
    await scan?.stop()
    await manager.destroy()
  }
}, 30000)
