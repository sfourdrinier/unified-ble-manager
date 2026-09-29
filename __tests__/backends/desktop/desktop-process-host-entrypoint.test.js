'use strict'

const h = require('../../helpers/desktop-rust-core-harness')
const electron = require('../../../src/electron-main')

test('Electron main reuses the same explicit process factory and actual native binding', async () => {
  const platform = { darwin: 'corebluetooth', linux: 'bluez', win32: 'winrt' }[process.platform]
  const name = {
    corebluetooth: 'createCoreBluetoothProcessHost',
    bluez: 'createBluezProcessHost',
    winrt: 'createWinRtProcessHost'
  }[platform]
  const node = require(`../../../src/node-${platform}`)
  expect(electron[name]).toBe(node[name])
  const harness = h.realBinding(platform)
  const host = await electron[name]({ binding: harness.binding, owner: 'electron-process-host' })
  try {
    expect(harness.productionRequests.length).toBeGreaterThan(0)
    expect(harness.productionRequests.every(request => request.platform === platform)).toBe(true)
    const before = harness.opened.length
    const first = await host.createInternalManager()
    const second = await host.createManager()
    expect(harness.opened).toHaveLength(before)
    expect(await first.destroy()).toMatchObject({ state: 'released' })
    expect(await second.destroy()).toMatchObject({ state: 'released' })
    expect(await host.continuation.status()).toBeNull()
  } finally {
    expect(await host.destroy()).toMatchObject({ state: 'released' })
  }
  await expect(host.continuationAccess.execute('unused', '{}')).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
})
