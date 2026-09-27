const { createIpcPeerDirectory, decodePeerQuery, encodePeerRecord } = require('../src/ipc/peer-directory')
const { contractError } = require('../src/backend-contract/errors')

const reference = { version: 1, backendId: 'corebluetooth', scope: 'system', opaqueId: 'os-guid' }
const record = () => ({
  reference,
  peerId: 'os-guid',
  name: 'Connected without advertisement',
  rssi: null,
  source: 'system-connected',
  state: { reachability: 'reachable', connection: 'connected', bond: 'unknown', lastSeenAtMonotonicMs: null }
})

test.each(['services', 'sources', 'references'])('sparse %s queries reject every missing entry', field => {
  expect(() => decodePeerQuery({ [field]: new Array(1) })).toThrow()
})

test('sparse peer responses reject instead of silently dropping an undecoded record', async () => {
  const peers = createIpcPeerDirectory({ route: async () => ({ peers: new Array(1) }) })
  await expect(peers.connected()).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('all directory categories use the existing IPC route and preserve genuine OS facts', async () => {
  const route = jest.fn(async command => (command === 'peers.resolve' ? { peer: record() } : { peers: [record()] }))
  const peers = createIpcPeerDirectory({ route })
  for (const category of ['known', 'connected', 'bonded', 'authorized', 'restored']) {
    const result = await peers[category]({
      services: ['180d'],
      sources: ['system-connected'],
      references: [reference],
      includeUnavailable: false,
      timeoutMs: 1000
    })
    expect(result[0]).toMatchObject({
      id: 'os-guid',
      reference,
      rssi: null,
      sources: ['system-connected'],
      state: { connection: 'connected', lastSeenAtMonotonicMs: null }
    })
    expect(route.mock.calls.at(-1)[0]).toBe(`peers.${category}`)
    expect(route.mock.calls.at(-1)[1].query).toEqual({
      services: ['0000180d-0000-1000-8000-00805f9b34fb'],
      sources: ['system-connected'],
      references: [reference],
      includeUnavailable: false
    })
  }
  await expect(peers.resolve(reference)).resolves.toMatchObject({ id: 'os-guid' })
  expect(route.mock.calls.at(-1)[1].reference).toEqual(reference)
})

test('null resolution and host unsupported/error causes cross without fallback', async () => {
  const route = jest.fn(async () => ({ peer: null }))
  const peers = createIpcPeerDirectory({ route })
  await expect(peers.resolve(reference)).resolves.toBeNull()
  const platform = {
    domain: 'CoreBluetooth',
    code: 'service-filter-required',
    safeMessage: 'A service is required',
    metadata: { requiredQueryField: 'services' }
  }
  route.mockRejectedValue(contractError('capability.unsupported', 'connection', 'native.connected', platform))
  await expect(peers.connected()).rejects.toMatchObject({
    code: 'capability.unsupported',
    operation: 'native.connected',
    platform
  })
  expect(route).toHaveBeenCalledTimes(2)
})

test('untrusted record and query fields fail closed without fabricated timestamps', async () => {
  for (const change of [
    r => (r.reference = { ...reference, version: 2 }),
    r => (r.source = 'invented'),
    r => (r.rssi = NaN),
    r => (r.state.connection = 'maybe'),
    r => (r.state.lastSeenAtMonotonicMs = 10)
  ]) {
    const value = record()
    change(value)
    await expect(createIpcPeerDirectory({ route: async () => ({ peers: [value] }) }).connected()).rejects.toBeDefined()
  }
  for (const query of [
    { services: [1] },
    { sources: ['invented'] },
    { references: [{}] },
    { includeUnavailable: 'yes' },
    { arbitrary: true }
  ]) {
    expect(() => decodePeerQuery(query)).toThrow()
  }
  expect(encodePeerRecord(record())).toEqual(record())
})

test('public cancellation and deadline bound an uncooperative IPC read and retain late rejection observation', async () => {
  jest.useFakeTimers()
  try {
    let reject
    const route = jest.fn(
      () =>
        new Promise((_resolve, rejected) => {
          reject = rejected
        })
    )
    const peers = createIpcPeerDirectory({ route })
    const abort = new AbortController()
    const pending = peers.connected({ signal: abort.signal })
    const rejected = expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
    abort.abort()
    await rejected
    reject(Error('late native refusal'))
    await Promise.resolve()
    const timed = expect(peers.connected({ timeoutMs: 20 })).rejects.toMatchObject({ code: 'operation.timed-out' })
    await jest.advanceTimersByTimeAsync(20)
    await timed
    reject(Error('late timeout rejection'))
    await Promise.resolve()
    const pre = new AbortController()
    pre.abort()
    await expect(peers.connected({ signal: pre.signal })).rejects.toMatchObject({ code: 'operation.aborted' })
    expect(route).toHaveBeenCalledTimes(2)
  } finally {
    jest.useRealTimers()
  }
})
