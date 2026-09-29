'use strict'

// Actual addon directory serialization, public provider and Rust synthetic
// connection owner. This is deterministic evidence, not physical radio proof.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const ID = '00e2ce71-3ba4-6569-e3de-3081ce0c95fb'

async function fixture(heldQuery) {
  const harness = h.realBinding('corebluetooth')
  const open = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (...args) => {
    const central = await open(...args)
    return new Proxy(central, {
      get(target, key) {
        if (key === 'connectedPeers' && heldQuery !== undefined) return heldQuery
        return Reflect.get(target, key)
      }
    })
  }
  const now = () => performance.now()
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'corebluetooth',
    owner: 'peer-directory-provider',
    now,
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: 'darwin'
  })
  const internal = await createNodeBleManagerFromProvider(
    provider,
    DESKTOP_RUST_CORE_PROFILES.corebluetooth.compatibility,
    { now }
  )
  const manager = await createPublicBleManager(internal, now)
  const stage = harness.opened.at(-1)
  await stage.stageDirectoryPeers([{ peerId: ID }])
  return { manager, harness, stage }
}

test('directory lookup acquires no lease; explicit public owners share and release independently', async () => {
  const { manager, harness, stage } = await fixture()
  const calls = name => harness.calls.filter(([method]) => method === name)
  try {
    await stage.stageServices(ID, h.hrmServices())
    expect(manager.capabilities.supports('peer:system-connected')).toBe(true)
    const [peer] = await manager.peers.connected({ services: ['180d'], timeoutMs: 1000 })
    expect(calls('connectedPeers')[0][1][0]).toMatchObject({
      ticket: expect.any(String),
      timeoutMs: expect.any(Number)
    })
    expect(peer).toMatchObject({ name: null, state: { connection: 'connected' } })
    expect(calls('startScan')).toHaveLength(0)
    expect(calls('connect')).toHaveLength(0)
    expect(calls('disconnect')).toHaveLength(0)
    const a = await manager.connect(peer)
    const b = await manager.connect(peer)
    expect(calls('connect')).toHaveLength(1)
    expect(calls('connect')[0][1][0].peerId).toBe(ID)
    expect(await a.release()).toEqual({ state: 'released', failures: [] })
    expect(calls('disconnect')).toHaveLength(0)
    const gatt = await b.discover()
    expect(await gatt.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT).read()).toEqual(new Uint8Array([0x42]))
    expect(await b.release()).toEqual({ state: 'released', failures: [] })
    expect(calls('disconnect')).toHaveLength(1)
  } finally {
    expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
  }
})

test('resolve reaches the same native identity and late completion after destroy cannot publish', async () => {
  let complete
  // Only this held-query edge is doubled; resolution still crosses real N-API.
  const { manager, harness } = await fixture(
    () =>
      new Promise(done => {
        complete = done
      })
  )
  const reference = { version: 1, backendId: 'unified-ble:corebluetooth', scope: 'application', opaqueId: ID }
  expect(await manager.peers.resolve(reference)).toMatchObject({
    reference,
    name: null,
    state: { connection: 'unknown' }
  })
  expect(harness.calls.find(([method]) => method === 'resolvePeer')[1][0]).toMatchObject({
    peerId: ID,
    ticket: expect.any(String)
  })
  const pending = manager.peers.connected({ services: ['180d'] })
  const checked = expect(pending).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
  expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
  complete([{ peerId: ID, name: null, connection: 'connected' }])
  await checked
})
