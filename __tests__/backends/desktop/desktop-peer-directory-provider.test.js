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

test.each([
  ['winrt', 'AA:BB:CC:DD:EE:FF', 'win32'],
  ['bluez', 'hci0/dev_AA_BB_CC_DD_EE_FF', 'linux']
])(
  'public bonded reference resolves the independent known OS route before explicit %s connection',
  async (platform, peerId, hostPlatform) => {
    const harness = h.realBinding(platform)
    const open = harness.binding.openSynthetic
    let records = [{ peerId, name: 'saved H10', connection: 'disconnected' }]
    const bonded = jest.fn(async () => records)
    const resolve = jest.fn(async id => records.find(record => record.peerId === id) ?? null)
    harness.binding.openSynthetic = async (...args) => {
      const central = await open(...args)
      return new Proxy(central, {
        get(target, key) {
          return key === 'bondedPeers'
            ? bonded
            : key === 'resolvePeer'
              ? options => resolve(options.peerId)
              : Reflect.get(target, key)
        }
      })
    }
    const now = () => performance.now()
    const provider = createTestDesktopRustCoreBackendProvider({
      platform,
      owner: 'bonded-roundtrip',
      now,
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform
    })
    const internal = await createNodeBleManagerFromProvider(
      provider,
      DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
      { now }
    )
    const manager = await createPublicBleManager(internal, now)
    try {
      const [peer] = await manager.peers.bonded()
      expect(harness.calls.filter(([method]) => method === 'connect')).toHaveLength(0)
      const connection = await manager.connect(peer.reference, { timeoutMs: 1000 })
      expect(bonded).toHaveBeenCalledTimes(1)
      expect(resolve).toHaveBeenCalledTimes(1)
      expect(harness.calls.filter(([method]) => method === 'resolvePeer')).toHaveLength(0)
      expect(harness.calls.filter(([method]) => method === 'connect')).toHaveLength(1)
      expect(await connection.release()).toEqual({ state: 'released', failures: [] })
      records = []
      expect(await manager.peers.resolve(peer.reference)).toBeNull()
      expect(manager.capabilities.get('peer:known').state).toBe('limited')
      expect(resolve).toHaveBeenCalledTimes(2)
    } finally {
      expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
    }
  }
)

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
    expect(manager.capabilities.get('peer:known').limitations.map(row => row.code)).toContain('references-required')
    expect(manager.capabilities.get('peer:system-connected').limitations.map(row => row.code)).toContain(
      'services-required'
    )
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

test.each([
  ['winrt', 'public:AA:BB:CC:DD:EE:FF', 'win32'],
  ['bluez', 'hci0/dev_AA_BB_CC_DD_EE_FF', 'linux']
])(
  '%s public known/system directory joins the actual addon without creating a lease',
  async (platform, peerId, hostPlatform) => {
    const harness = h.realBinding(platform)
    const now = () => performance.now()
    const provider = createTestDesktopRustCoreBackendProvider({
      platform,
      owner: 'native-directories',
      now,
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform
    })
    const manager = await createPublicBleManager(
      await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES[platform].compatibility, { now }),
      now
    )
    const stage = harness.opened.at(-1)
    try {
      expect(manager.capabilities.get('peer:known').limitations.map(row => row.code)).not.toContain(
        'references-required'
      )
      expect(manager.capabilities.get('peer:system-connected').limitations.map(row => row.code)).not.toContain(
        'services-required'
      )
      await stage.stageKnownDirectoryPeers([{ peerId, name: 'unbonded OS-visible peer' }])
      await stage.stageDirectoryPeers([{ peerId, name: 'foreign app connected peer' }])
      const [known] = await manager.peers.known()
      expect(known).toMatchObject({
        name: 'unbonded OS-visible peer',
        sources: ['backend-cache'],
        state: { connection: 'unknown' }
      })
      const [connected] = await manager.peers.connected()
      expect(connected).toMatchObject({
        name: 'foreign app connected peer',
        sources: ['system-connected'],
        state: { connection: 'connected' }
      })
      expect(await manager.peers.resolve(known.reference)).not.toBeNull()
      for (const method of ['connect', 'startScan', 'pair'])
        expect(harness.calls.filter(([name]) => name === method)).toHaveLength(0)
      const records = await stage.peerRecords()
      expect(records).toEqual([])
      await stage.stageKnownDirectoryPeers([])
      await stage.stageDirectoryPeers([])
      expect(await manager.peers.known()).toEqual([])
      expect(await manager.peers.resolve(known.reference)).toBeNull()
    } finally {
      await manager.destroy()
    }
  },
  30000
)
