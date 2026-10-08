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

test('initial probe failure without a newer observation closes the source and retains the refusal', async () => {
  const fixture = pendingProbe()
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
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

test.each([0, NaN, Infinity, 1.5, Number.MAX_SAFE_INTEGER + 1])(
  'invalid initial parameter interval %s fails acquisition',
  async intervalUs => {
    const fixture = pendingProbe()
    const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
    fixture.resolve({ intervalUs, latency: 0, supervisionTimeoutUs: 4_000_000 })
    await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
    expect(fixture.backend.parameterWatches.size).toBe(0)
  }
)

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

test('a fresh watch retries transient source failure without waiting for a changed event', async () => {
  const fixture = pendingProbe()
  fixture.backend.applyConnectionParameters({
    kind: 'source-failed',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    error: 'platform.failure|platform|native.parameter-getter|never|||transient'
  })
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.resolve({ intervalUs: 90_000, latency: 2, supervisionTimeoutUs: 4_000_000 })
  const watch = await opening
  expect(fixture.record.parameterSourceFailure).toBeNull()
  expect((await watch.events[Symbol.asyncIterator]().next()).value.value.intervalUs).toBe(90_000)
  await watch.close()
})

test('a newer identical transient failure beats a pending recovery probe', async () => {
  const fixture = pendingProbe()
  const failure = {
    kind: 'source-failed',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    error: 'platform.failure|platform|native.parameter-getter|never|||transient'
  }
  fixture.backend.applyConnectionParameters(failure)
  const opening = fixture.backend.watchConnectionParameters(fixture.record.path)
  fixture.backend.applyConnectionParameters(failure)
  fixture.resolve({ intervalUs: 90_000, latency: 2, supervisionTimeoutUs: 4_000_000 })
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'platform.failure' } })
  expect(fixture.record.parameterSourceFailure.code).toBe('platform.failure')
})

test.each([false, true])('readiness reprobe discards stale %s outcome after a newer event', async failed => {
  const fixture = pendingProbe()
  fixture.backend.readinessWatches = new Set()
  fixture.backend.destroyed = false
  fixture.backend.central.writeReadiness = async () => false
  const publicWatch = await fixture.backend.writeWithoutResponseReadiness(fixture.record.path)
  const owned = [...fixture.backend.readinessWatches][0]
  let settle, refuse
  fixture.backend.central.writeReadiness = () =>
    new Promise((yes, no) => {
      settle = yes
      refuse = no
    })
  const probing = fixture.backend.reprobeReadiness(owned)
  fixture.backend.applyWriteReadiness({
    kind: 'state',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    ready: true
  })
  if (failed) refuse(new Error('stale failure'))
  else settle(false)
  await probing
  expect(owned.ready).toBe(true)
  expect(owned.ordinal).toBe(2)
  expect(fixture.backend.readinessWatches.has(owned)).toBe(true)
  await publicWatch.close()
})

test('queued native readiness during a pending gap probe supersedes the getter before publication', async () => {
  const fixture = pendingProbe()
  fixture.backend.readinessWatches = new Set()
  fixture.backend.destroyed = false
  fixture.backend.noteDiagnostic = () => {}
  fixture.backend.central.writeReadiness = async () => false
  const watch = await fixture.backend.writeWithoutResponseReadiness(fixture.record.path)
  const owned = [...fixture.backend.readinessWatches][0]
  let settle
  fixture.backend.central.writeReadiness = () =>
    new Promise(resolve => {
      settle = resolve
    })
  const events = []
  fixture.backend.central.takeWriteReadinessEvent = async () => events.shift() ?? null
  const probing = fixture.backend.reconcileWriteReadiness(1)
  events.push({ kind: 'state', peerId: 'peer', connectionGeneration: 'native-generation', ready: true })
  settle(false)
  await probing
  expect(owned.ready).toBe(true)
  expect(owned.ordinal).toBe(2)
  await watch.close()
})

test('parameter reconciliation preserves a queued source failure before publishing the healthy scalar', async () => {
  const fixture = pendingProbe()
  fixture.backend.noteDiagnostic = () => {}
  fixture.resolve({ intervalUs: 30_000, latency: 2, supervisionTimeoutUs: 4_000_000 })
  const publicWatch = await fixture.backend.watchConnectionParameters(fixture.record.path)
  const watch = [...fixture.backend.parameterWatches][0]
  fixture.backend.central.connectionParameters = async () => ({
    intervalUs: 90_000,
    latency: 2,
    supervisionTimeoutUs: 4_000_000
  })
  const events = [
    {
      kind: 'source-failed',
      peerId: 'peer',
      connectionGeneration: 'native-generation',
      error: 'platform.failure|platform|original.callback|never|||retained cause'
    }
  ]
  fixture.backend.central.takeConnectionParameterEvent = async () => events.shift() ?? null
  await fixture.backend.reconcileConnectionParameters(1)
  expect(watch.ordinal).toBe(1)
  expect(watch.failure).toMatchObject({
    code: 'platform.failure',
    platform: { metadata: { coreOperation: 'original.callback' } }
  })
  expect(fixture.backend.parameterWatches.size).toBe(0)
  await publicWatch.close()
})

test.each([
  { intervalUs: 1.5 },
  { intervalUs: Number.MAX_SAFE_INTEGER + 1 },
  { supervisionTimeoutUs: 1.5 },
  { supervisionTimeoutUs: Number.MAX_SAFE_INTEGER + 1 }
])('malformed native microseconds %j refuse snapshot acquisition and live events', async invalid => {
  const initial = pendingProbe()
  const opening = initial.backend.watchConnectionParameters(initial.record.path)
  initial.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000, ...invalid })
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
  expect(initial.backend.parameterWatches.size).toBe(0)
  const live = pendingProbe()
  live.resolve({ intervalUs: 30_000, latency: 0, supervisionTimeoutUs: 4_000_000 })
  const watch = await live.backend.watchConnectionParameters(live.record.path)
  const iterator = watch.events[Symbol.asyncIterator]()
  await iterator.next()
  live.backend.applyConnectionParameters({
    kind: 'state',
    peerId: 'peer',
    connectionGeneration: 'native-generation',
    intervalUs: 30_000,
    latency: 0,
    supervisionTimeoutUs: 4_000_000,
    ...invalid
  })
  await expect(iterator.next()).resolves.toMatchObject({
    value: { kind: 'terminal', reason: 'source-failed', error: { code: 'protocol.violation' } }
  })
  await watch.close()
})

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}
const parameters = intervalUs => ({ intervalUs, latency: 2, supervisionTimeoutUs: 4_000_000 })

function openingRace(kind) {
  const fixture = pendingProbe()
  fixture.backend.noteDiagnostic = () => {}
  fixture.backend.destroyed = false
  fixture.backend.readinessWatches = new Set()
  const initial = deferred()
  const recovery = deferred()
  const method = kind === 'parameters' ? 'connectionParameters' : 'writeReadiness'
  fixture.backend.central[method] = jest.fn().mockReturnValueOnce(initial.promise).mockReturnValueOnce(recovery.promise)
  const opening =
    kind === 'parameters'
      ? fixture.backend.watchConnectionParameters(fixture.record.path)
      : fixture.backend.writeWithoutResponseReadiness(fixture.record.path)
  const owned = [...(kind === 'parameters' ? fixture.backend.parameterWatches : fixture.backend.readinessWatches)][0]
  const recovering = () =>
    kind === 'parameters'
      ? fixture.backend.reconcileConnectionParameters(1)
      : fixture.backend.reconcileWriteReadiness(1)
  const event = () =>
    kind === 'parameters'
      ? fixture.event(90_000)
      : fixture.backend.applyWriteReadiness({
          kind: 'state',
          peerId: 'peer',
          connectionGeneration: 'native-generation',
          ready: true
        })
  return { ...fixture, initial, recovery, opening, owned, recovering, event }
}

test.each(['parameters', 'readiness'])('%s opening ignores a stale getter after a newer recovery', async kind => {
  const f = openingRace(kind)
  const recovery = f.recovering()
  f.recovery.resolve(kind === 'parameters' ? parameters(90_000) : true)
  await recovery
  f.initial.resolve(kind === 'parameters' ? parameters(30_000) : false)
  const watch = await f.opening
  const iterator = watch.events[Symbol.asyncIterator]()
  expect((await iterator.next()).value.value).toMatchObject(
    kind === 'parameters' ? { intervalUs: 90_000 } : { ready: true }
  )
  expect(f.owned.ordinal).toBe(1)
  await watch.close()
})

test.each(['parameters', 'readiness'])(
  '%s buffered event invalidates stale recovery and opening successes',
  async kind => {
    const f = openingRace(kind)
    const recovery = f.recovering()
    f.event()
    f.recovery.resolve(kind === 'parameters' ? parameters(60_000) : false)
    await recovery
    f.initial.resolve(kind === 'parameters' ? parameters(30_000) : false)
    const watch = await f.opening
    expect((await watch.events[Symbol.asyncIterator]().next()).value.value).toMatchObject(
      kind === 'parameters' ? { intervalUs: 90_000 } : { ready: true }
    )
    expect(f.owned.ordinal).toBe(1)
    await watch.close()
  }
)

test.each(['parameters', 'readiness'])(
  '%s buffered event invalidates stale recovery and opening failures',
  async kind => {
    const f = openingRace(kind)
    const recovery = f.recovering()
    f.event()
    f.recovery.reject(new Error('stale recovery failure'))
    await recovery
    f.initial.reject(new Error('stale opening failure'))
    const watch = await f.opening
    expect((await watch.events[Symbol.asyncIterator]().next()).value.value).toMatchObject(
      kind === 'parameters' ? { intervalUs: 90_000 } : { ready: true }
    )
    expect(f.owned.ordinal).toBe(1)
    await watch.close()
  }
)

test.each(['parameters', 'readiness'])(
  '%s generation replacement during opening never resurrects the old watch',
  async kind => {
    const f = openingRace(kind)
    f.event()
    f.record.coreGeneration = 'replacement'
    f.initial.resolve(kind === 'parameters' ? parameters(30_000) : false)
    await expect(f.opening).rejects.toMatchObject({ normalized: { code: 'connection.stale' } })
    expect((kind === 'parameters' ? f.backend.parameterWatches : f.backend.readinessWatches).size).toBe(0)
  }
)

test.each(['parameters', 'readiness'])(
  '%s genuine recovery failure with no newer state closes acquisition',
  async kind => {
    const f = openingRace(kind)
    const recovery = f.recovering()
    f.recovery.reject(new Error('platform.failure|platform|native.recovery|never|||current recovery failure'))
    await recovery
    f.initial.resolve(kind === 'parameters' ? parameters(30_000) : false)
    await expect(f.opening).rejects.toMatchObject({ normalized: { code: 'platform.failure' } })
    expect((kind === 'parameters' ? f.backend.parameterWatches : f.backend.readinessWatches).size).toBe(0)
  }
)

test.each(['parameters', 'readiness'])('%s caller cancellation wins over buffered source state', async kind => {
  const f = pendingProbe()
  f.backend.readinessWatches = new Set()
  const controller = new AbortController()
  f.backend.central.writeReadiness = () => f.backend.central.connectionParameters()
  const opening =
    kind === 'parameters'
      ? f.backend.watchConnectionParameters(f.record.path, { signal: controller.signal, deadline: null })
      : f.backend.writeWithoutResponseReadiness(f.record.path, { signal: controller.signal, deadline: null })
  if (kind === 'parameters') f.event(90_000)
  else
    f.backend.applyWriteReadiness({
      kind: 'state',
      peerId: 'peer',
      connectionGeneration: 'native-generation',
      ready: true
    })
  controller.abort()
  f.resolve(kind === 'parameters' ? parameters(30_000) : false)
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  expect((kind === 'parameters' ? f.backend.parameterWatches : f.backend.readinessWatches).size).toBe(0)
})

test.each(['parameters', 'readiness'])('%s accepted recovery also supersedes an older opening failure', async kind => {
  const f = openingRace(kind)
  const recovery = f.recovering()
  f.recovery.resolve(kind === 'parameters' ? parameters(90_000) : true)
  await recovery
  f.initial.reject(new Error('obsolete opening refusal'))
  const watch = await f.opening
  expect((await watch.events[Symbol.asyncIterator]().next()).value.value).toMatchObject(
    kind === 'parameters' ? { intervalUs: 90_000 } : { ready: true }
  )
  expect(f.owned.ordinal).toBe(1)
  await watch.close()
})

test.each(['parameters', 'readiness'])('%s expired admission does not borrow a buffered healthy state', async kind => {
  const f = pendingProbe()
  f.backend.readinessWatches = new Set()
  let time = 0
  f.backend.now = () => time
  f.backend.central.writeReadiness = () => f.backend.central.connectionParameters()
  const opening =
    kind === 'parameters'
      ? f.backend.watchConnectionParameters(f.record.path, { signal: null, deadline: 10 })
      : f.backend.writeWithoutResponseReadiness(f.record.path, { signal: null, deadline: 10 })
  if (kind === 'parameters') f.event(90_000)
  else
    f.backend.applyWriteReadiness({
      kind: 'state',
      peerId: 'peer',
      connectionGeneration: 'native-generation',
      ready: true
    })
  time = 11
  f.resolve(kind === 'parameters' ? parameters(30_000) : false)
  await expect(opening).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
  expect((kind === 'parameters' ? f.backend.parameterWatches : f.backend.readinessWatches).size).toBe(0)
})

test.each(['parameters', 'readiness'])(
  '%s publishing an already accepted buffer does not invalidate a newer probe',
  async kind => {
    const f = openingRace(kind)
    f.event()
    const recovery = f.recovering()
    f.initial.resolve(kind === 'parameters' ? parameters(30_000) : false)
    const watch = await f.opening
    const iterator = watch.events[Symbol.asyncIterator]()
    expect((await iterator.next()).value.value).toMatchObject(
      kind === 'parameters' ? { intervalUs: 90_000 } : { ready: true }
    )
    expect(f.owned.acceptanceRevision).toBe(1)
    f.recovery.resolve(kind === 'parameters' ? parameters(120_000) : false)
    await recovery
    expect((await iterator.next()).value.value).toMatchObject(
      kind === 'parameters' ? { intervalUs: 120_000 } : { ready: false }
    )
    expect(f.owned.ordinal).toBe(2)
    await watch.close()
  }
)
