'use strict'

const { createDesktopPeerDirectory } = require('../../../src/backends/desktop/desktop-peer-directory')
const { createPublicPeerDirectory } = require('../../../src/public/peer-directory')
const { contractError } = require('../../../src/backend-contract/errors')

const ID = '00e2ce71-3ba4-6569-e3de-3081ce0c95fb'
const OTHER = '00e2ce71-3ba4-6569-e3de-3081ce0c95fc'
const BACKEND = 'unified-ble:corebluetooth'
const reference = opaqueId => ({ version: 1, backendId: BACKEND, scope: 'application', opaqueId })

function fixture() {
  const hooks = {
    backendId: BACKEND,
    generation: () => 1,
    connected: jest.fn(async () => [{ peerId: ID.toUpperCase(), name: 'SIM Polar H10', connection: 'connected' }]),
    resolve: jest.fn(async peerId => ({ peerId, name: null, connection: 'unknown' })),
    peerId: jest.fn(peerId => `mapped:${peerId}`),
    assertUsable: jest.fn()
  }
  const backend = createDesktopPeerDirectory(hooks)
  return { hooks, backend, peers: createPublicPeerDirectory(backend, () => performance.now()) }
}

test('connected membership is reported without acquiring a connection or fabricating observations', async () => {
  const { peers, hooks } = fixture()
  const [peer] = await peers.connected({ services: ['180D'] })
  expect(hooks.connected).toHaveBeenCalledWith(['0000180d-0000-1000-8000-00805f9b34fb'], expect.any(Object))
  expect(peer).toMatchObject({
    id: `mapped:${ID}`,
    name: 'SIM Polar H10',
    rssi: null,
    reference: reference(ID),
    sources: ['system-connected'],
    lastAdvertisement: null,
    state: { connection: 'connected', reachability: 'unknown', bond: 'unsupported', lastSeenAtMonotonicMs: null }
  })
  expect(hooks.resolve).not.toHaveBeenCalled()
})

test.each([
  ['unified-ble:winrt', 'AA:BB:CC:DD:EE:FF'],
  ['unified-ble:bluez-dbus', 'hci0/dev_AA_BB_CC_DD_EE_FF']
])('bonded directory preserves native identities and no-link facts for %s', async (backendId, peerId) => {
  const hooks = {
    backendId,
    generation: () => 1,
    assertUsable: jest.fn(),
    peerId: id => `mapped:${id}`,
    bonded: jest.fn(async () => [{ peerId, name: 'saved H10', connection: 'disconnected' }]),
    resolveFromBonded: true,
    connected: jest.fn(),
    resolve: jest.fn()
  }
  const directory = createDesktopPeerDirectory(hooks)
  const [record] = await directory.bonded({})
  expect(record).toMatchObject({
    reference: { version: 1, backendId, scope: 'application', opaqueId: peerId },
    peerId: `mapped:${peerId}`,
    source: 'system-bonded',
    state: { bond: 'bonded', connection: 'disconnected', reachability: 'unknown' }
  })
  expect(await directory.bonded({ sources: ['scan-observed'] })).toEqual([])
  expect(
    await directory.bonded({ references: [{ version: 1, backendId, scope: 'application', opaqueId: peerId }] })
  ).toHaveLength(1)
  await expect(directory.bonded({ services: ['180d'] })).rejects.toThrow(
    'capability.unsupported: peers.bonded.services'
  )
  expect(hooks.resolve).not.toHaveBeenCalled()
  await expect(directory.known({ references: [record.reference] })).rejects.toThrow(
    'capability.unsupported: peers.known'
  )
  expect(await directory.resolve(record.reference, {})).toMatchObject({
    source: 'system-bonded',
    state: { bond: 'bonded' }
  })
  hooks.bonded.mockResolvedValue([])
  expect(await directory.resolve(record.reference, {})).toBeNull()
  expect(hooks.resolve).not.toHaveBeenCalled()
})

test.each([undefined, []])('connected query requires an explicit service filter (%p)', async services => {
  const { peers, hooks } = fixture()
  await expect(peers.connected({ services })).rejects.toMatchObject({
    code: 'capability.unsupported',
    operation: 'peers.connected.services-required'
  })
  expect(hooks.connected).not.toHaveBeenCalled()
})

test('resolve validates identity, preserves null, and never claims an owned connection', async () => {
  const { peers, hooks } = fixture()
  expect(await peers.resolve(reference(ID))).toMatchObject({
    reference: reference(ID),
    sources: ['app-reference'],
    state: { connection: 'unknown' }
  })
  hooks.resolve.mockResolvedValueOnce(null)
  expect(await peers.resolve(reference(OTHER))).toBeNull()
  expect(hooks.connected).not.toHaveBeenCalled()
})

test.each([{ ...reference(ID), backendId: 'another' }, { ...reference(ID), scope: 'system' }, reference('not-a-guid')])(
  'invalid or foreign reference refuses before native dispatch: %p',
  async ref => {
    const { peers, hooks } = fixture()
    await expect(peers.resolve(ref)).rejects.toMatchObject({ code: expect.stringMatching(/^peer\./u) })
    expect(hooks.resolve).not.toHaveBeenCalled()
  }
)

test('known resolves supplied references only; never fabricates a system-wide cache', async () => {
  const { peers, hooks } = fixture()
  await expect(peers.known()).rejects.toMatchObject({ code: 'capability.unsupported' })
  expect(hooks.resolve).not.toHaveBeenCalled()
  expect(await peers.known({ references: [reference(ID), reference(ID)] })).toHaveLength(1)
  expect(hooks.resolve).toHaveBeenCalledTimes(1)
  await expect(peers.known({ references: [reference(ID)], services: ['180d'] })).rejects.toMatchObject({
    code: 'capability.unsupported'
  })
})

test('sources and references filter real connected records without inventing matches', async () => {
  const { peers } = fixture()
  expect(await peers.connected({ services: ['180d'], references: [reference(OTHER)] })).toEqual([])
  expect(await peers.connected({ services: ['180d'], sources: ['system-bonded'] })).toEqual([])
})

test.each(['bonded', 'authorized', 'restored'])('%s remains explicitly unsupported', async method => {
  const { peers } = fixture()
  await expect(peers[method]()).rejects.toMatchObject({ code: 'capability.unsupported' })
})

test('native failures retain their specific cause', async () => {
  const { peers, hooks } = fixture()
  hooks.connected.mockRejectedValueOnce(contractError('permission.denied', 'platform', 'native.query'))
  await expect(peers.connected({ services: ['180d'] })).rejects.toMatchObject({
    code: 'permission.denied',
    operation: 'native.query'
  })
})

test('abort while lookup is pending rejects the public wait and observes late native failure', async () => {
  const { peers, hooks } = fixture()
  let reject
  hooks.connected.mockImplementationOnce(
    () =>
      new Promise((_, fail) => {
        reject = fail
      })
  )
  const abort = new AbortController()
  const pending = peers.connected({ services: ['180d'], signal: abort.signal })
  abort.abort()
  await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
  reject(contractError('permission.denied', 'platform', 'late.query'))
  await new Promise(resolve => setImmediate(resolve))
  expect(hooks.peerId).not.toHaveBeenCalled()
})

test('lookup result cannot publish peer mappings after its owner is invalidated', async () => {
  const { peers, hooks } = fixture()
  hooks.connected.mockImplementationOnce(async () => {
    hooks.assertUsable.mockImplementation(() => {
      throw contractError('lifecycle.destroyed', 'core', 'peers.connected')
    })
    return [{ peerId: ID, name: null, connection: 'connected' }]
  })
  await expect(peers.connected({ services: ['180d'] })).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
  expect(hooks.peerId).not.toHaveBeenCalled()
})

test.each([
  { peerId: 'garbage', name: null, connection: 'connected' },
  { peerId: ID, name: undefined, connection: 'connected' },
  { peerId: ID, name: null, connection: 'disconnected' }
])('malformed or contradictory connected response is refused before mapping: %p', async record => {
  const { peers, hooks } = fixture()
  hooks.connected.mockResolvedValueOnce([{ peerId: ID, name: 'valid', connection: 'connected' }, record])
  await expect(peers.connected({ services: ['180d'] })).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(hooks.peerId).not.toHaveBeenCalled()
})

test('resolve refuses a different identity instead of silently rebinding a saved reference', async () => {
  const { peers, hooks } = fixture()
  hooks.resolve.mockResolvedValueOnce({ peerId: OTHER, name: null, connection: 'unknown' })
  await expect(peers.resolve(reference(ID))).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(hooks.peerId).not.toHaveBeenCalled()
})

test('query filter is canonical, unique and disjunctive; provider receives every requested service', async () => {
  const { peers, hooks } = fixture()
  await peers.connected({ services: ['180d', '180D', '180f'] })
  expect(hooks.connected.mock.calls[0][0]).toEqual([
    '0000180d-0000-1000-8000-00805f9b34fb',
    '0000180f-0000-1000-8000-00805f9b34fb'
  ])
})

test.each([null, {}, 4])('invalid native array container is a protocol failure (%p)', async records => {
  const { peers, hooks } = fixture()
  hooks.connected.mockResolvedValueOnce(records)
  await expect(peers.connected({ services: ['180d'] })).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(hooks.peerId).not.toHaveBeenCalled()
})

test('known refuses a generation change between sequential resolutions', async () => {
  const { peers, hooks } = fixture()
  let generation = 1
  hooks.generation = () => generation
  hooks.peerId.mockImplementationOnce(peerId => {
    queueMicrotask(() => {
      generation = 2
    })
    return `mapped:${peerId}`
  })
  await expect(peers.known({ references: [reference(ID), reference(OTHER)] })).rejects.toMatchObject({
    code: 'lifecycle.invalid-state'
  })
  expect(hooks.resolve).toHaveBeenCalledTimes(1)
})
