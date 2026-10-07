const { DesktopRustCoreBackend } = require('../../../src/backends/desktop/desktop-rust-core-provider')

function pendingProbe(honorCancellation = false) {
  let resolve, reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  const record = {
    nativePeerId: 'peer',
    coreGeneration: 'native-generation',
    lease: 'lease',
    state: 'connected',
    path: { connectionId: 'connection', connectionGeneration: 'generation' }
  }
  let time = 0
  Object.assign(backend, {
    parameterWatches: new Set(),
    connectionsById: new Map([['connection', record]]),
    op: name => name,
    assertOperational: () => {},
    liveConnection: () => record,
    mintedCorrelation: () => 'op',
    withTicket: (_id, _signal, _operation, action) => action('ticket'),
    budget: () => ({}),
    now: () => ++time,
    central: { connectionParameters: () => promise }
  })
  if (honorCancellation)
    backend.withTicket = (_id, signal, _operation, action) => {
      const pending = action('ticket')
      return new Promise((resolve, reject) => {
        signal.addEventListener('abort', () => reject(new Error('native probe cancelled')), { once: true })
        pending.then(resolve, reject)
      })
    }
  const event = intervalUs =>
    backend.applyConnectionParameters({
      kind: 'state',
      peerId: 'peer',
      connectionGeneration: 'native-generation',
      intervalUs,
      latency: 2,
      supervisionTimeoutUs: 4_000_000
    })
  return { backend, record, resolve, reject, event }
}

test('a native getter failure during acquisition cancels the probe and promptly reports the source error', async () => {
  const fixture = pendingProbe(true)
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.backend.applyConnectionParameters({
    kind: 'source-failed',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    error: 'platform.failure|platform|native.parameter-getter|never|||getter failed'
  })
  await expect(opening).rejects.toMatchObject({
    normalized: {
      code: 'platform.failure',
      operation: 'connection.parameters.event',
      platform: { metadata: { coreOperation: 'native.parameter-getter' } }
    }
  })
  expect(fixture.backend.parameterWatches.size).toBe(0)
  fixture.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000 })
})

test('a failed native source refuses later watches until a fresh source observation recovers it', async () => {
  const fixture = pendingProbe()
  const getter = jest.spyOn(fixture.backend.central, 'connectionParameters')
  fixture.backend.applyConnectionParameters({
    kind: 'source-failed',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    error: 'stream.closed|stream|native.parameter-source|never|||source closed'
  })
  await expect(fixture.backend.watchConnectionParameters(fixture.record.path)).rejects.toMatchObject({
    normalized: {
      code: 'stream.closed',
      operation: 'connection.parameters.event',
      platform: { metadata: { coreOperation: 'native.parameter-source' } }
    }
  })
  expect(getter).not.toHaveBeenCalled()
  fixture.event(90_000)
  fixture.resolve({ intervalUs: 90_000, latency: 2, supervisionTimeoutUs: 4_000_000 })
  const watch = await fixture.backend.watchConnectionParameters(fixture.record.path)
  const first = await watch.events[Symbol.asyncIterator]().next()
  expect(first.value.value.intervalUs).toBe(90_000)
  expect(getter).toHaveBeenCalledTimes(1)
  await watch.close()
})

test('reports arriving during the initial probe replace its delayed answer and retain native order', async () => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.event(60_000)
  fixture.event(90_000)
  fixture.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000 })
  const watch = await opening
  const iterator = watch.events[Symbol.asyncIterator]()
  const values = []
  for (let index = 0; index < 2; index += 1) values.push((await iterator.next()).value.value)
  expect(values.map(value => value.intervalUs)).toEqual([60_000, 90_000])
  expect(values.map(value => value.ordinal)).toEqual([1, 2])
  expect(values.map(value => value.observedAtMonotonicMs)).toEqual(
    [...values.map(value => value.observedAtMonotonicMs)].sort((a, b) => a - b)
  )
  await watch.close()
  expect(fixture.backend.parameterWatches.size).toBe(0)
})

test('initial probe failure closes the registered source and retains the refusal', async () => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.event(60_000)
  fixture.reject(new Error('probe failed'))
  await expect(opening).rejects.toThrow('probe failed')
  expect(fixture.backend.parameterWatches.size).toBe(0)
})

test('disconnect during acquisition does not resurrect the watch or emit the stale probe', async () => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  const watch = [...fixture.backend.parameterWatches][0]
  fixture.record.state = 'disconnected'
  fixture.backend.closeParameterWatch(watch, 'connection-lost')
  fixture.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000 })
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'connection.stale' } })
  expect(fixture.backend.parameterWatches.size).toBe(0)
})

test.each([0, NaN, Infinity])('invalid initial parameter interval %s fails acquisition', async intervalUs => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.resolve({ intervalUs, latency: 0, supervisionTimeoutUs: 4_000_000 })
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
  expect(fixture.backend.parameterWatches.size).toBe(0)
})

test('a malformed report during acquisition retains its own source failure', async () => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.backend.applyConnectionParameters({
    kind: 'state',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    intervalUs: null,
    latency: 0,
    supervisionTimeoutUs: 4_000_000
  })
  fixture.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000 })
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
  expect(fixture.backend.parameterWatches.size).toBe(0)
})

test('initial buffering overflow reports the source fault rather than a stale connection', async () => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  for (let index = 0; index < 65; index += 1) fixture.event(60_000 + index)
  fixture.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000 })
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'stream.overflow' } })
  expect(fixture.backend.parameterWatches.size).toBe(0)
})
