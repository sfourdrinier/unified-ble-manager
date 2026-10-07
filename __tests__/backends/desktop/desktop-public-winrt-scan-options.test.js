'use strict'

const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

test('public Windows scan modes and extended opt-in reach the owned native scan', async () => {
  const harness = h.realBinding('winrt')
  const now = () => performance.now()
  const manager = await createPublicBleManager(
    await createNodeBleManagerFromProvider(
      createTestDesktopRustCoreBackendProvider({
        platform: 'winrt',
        owner: 'windows-scan-options',
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
  try {
    for (const mode of ['active', 'passive', 'none']) {
      const scan = await manager.scan({ platform: { kind: 'winrt', mode, allowExtendedAdvertisements: true } })
      expect(harness.calls.filter(([method]) => method === 'startScan').at(-1)[1][0]).toMatchObject({
        winrtScanningMode: mode,
        winrtAllowExtendedAdvertisements: true
      })
      await scan.stop()
    }
    const count = harness.calls.filter(([method]) => method === 'startScan').length
    await expect(manager.scan({ platform: { kind: 'winrt', mode: 'invalid' } })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    await expect(manager.scan({ platform: { kind: 'winrt', allowExtendedAdvertisements: 1 } })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(harness.calls.filter(([method]) => method === 'startScan')).toHaveLength(count)
    const scan = await manager.scan()
    const call = harness.calls.filter(([method]) => method === 'startScan').at(-1)[1][0]
    expect(call.winrtScanningMode).toBeUndefined()
    expect(call.winrtAllowExtendedAdvertisements).toBeUndefined()
    await scan.stop()
  } finally {
    await manager.destroy()
  }
}, 30000)
