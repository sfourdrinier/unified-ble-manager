const { AccessoryChooserScenario } = require('../../examples-shared/driver/scenarios/accessory-chooser.ts')

function fixture() {
  let deliver
  const pending = new Promise(resolve => {
    deliver = resolve
  })
  const iterator = { next: jest.fn(() => pending), return: jest.fn(async () => ({ done: true })) }
  const subscription = {
    values: { [Symbol.asyncIterator]: () => iterator },
    remove: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
  const characteristic = { subscribe: jest.fn(async () => subscription) }
  const database = { generation: 'db-1', services: [{ uuid: '180d' }], characteristic: jest.fn(() => characteristic) }
  const connection = {
    connectionGeneration: 'link-1',
    discover: jest.fn(async () => database),
    release: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
  const peer = { id: 'scoped-chooser-peer', name: 'SIM Polar H10 0001' }
  const manager = {
    discovery: { kind: 'system' },
    capabilities: { get: () => ({ status: 'supported' }) },
    choose: jest.fn(async () => peer),
    connect: jest.fn(async () => connection),
    find: jest.fn(),
    scan: jest.fn(),
    destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
  const host = {
    runtime: { host: 'test', now: () => Date.now(), schedule: () => () => {}, log: () => {} },
    identity: { backend: 'test' },
    appState: { current: () => ({ foreground: true }) },
    userGesture: null,
    createManager: jest.fn(async () => ({ manager, prepare: jest.fn(async () => {}) }))
  }
  return {
    scenario: new AccessoryChooserScenario(host),
    host,
    manager,
    peer,
    connection,
    database,
    characteristic,
    subscription,
    iterator,
    deliver
  }
}

async function connected(f) {
  await f.scenario.dispatch('choose', {})
  await f.scenario.dispatch('connect-selected', {})
}

test('chooser sample proves actual HRS bytes on the same scoped connection and removes ownership', async () => {
  const f = fixture()
  await connected(f)
  f.deliver({
    done: false,
    value: {
      kind: 'value',
      value: { value: new Uint8Array([0, 72]), delivery: 'notification', sequence: 3, observedAtMonotonicMs: 123 }
    }
  })
  const result = await f.scenario.dispatch('sample-selected-hr', { timeoutMs: 100 })
  expect(result).toMatchObject({
    peerId: f.peer.id,
    connectionGeneration: 'link-1',
    databaseGeneration: 'db-1',
    bytes: [0, 72],
    bpm: 72,
    delivery: 'notification',
    sequence: 3
  })
  expect(f.host.createManager).toHaveBeenCalledTimes(1)
  expect(f.manager.connect).toHaveBeenCalledWith(f.peer, expect.any(Object))
  expect(f.manager.find).not.toHaveBeenCalled()
  expect(f.manager.scan).not.toHaveBeenCalled()
  expect(f.database.characteristic).toHaveBeenCalledWith(
    '0000180d-0000-1000-8000-00805f9b34fb',
    '00002a37-0000-1000-8000-00805f9b34fb'
  )
  expect(f.subscription.remove).toHaveBeenCalledTimes(1)
  await f.scenario.stop()
  expect(f.subscription.remove).toHaveBeenCalledTimes(1)
})

test.each(['terminal', 'overflow', 'timeout', 'cancel'])(
  'sample reports %s rather than false success and releases subscription',
  async outcome => {
    const f = fixture()
    await connected(f)
    if (outcome === 'terminal' || outcome === 'overflow')
      f.deliver({
        done: false,
        value: {
          kind: 'terminal',
          reason: outcome === 'overflow' ? 'overflow' : 'source-failed',
          error: { code: outcome === 'overflow' ? 'stream.overflow' : 'platform.transport' }
        }
      })
    const sample = f.scenario.dispatch('sample-selected-hr', { timeoutMs: 15 })
    const failed = expect(sample).rejects.toMatchObject({
      code:
        outcome === 'terminal' || outcome === 'overflow'
          ? 'scenario.notification-terminal'
          : outcome === 'timeout'
            ? 'scenario.notification-timeout'
            : 'operation.aborted'
    })
    if (outcome === 'cancel') {
      await Promise.resolve()
      await f.scenario.stop()
    }
    await failed
    expect(f.subscription.remove).toHaveBeenCalled()
  }
)

test('stopped selection cannot acquire a new subscription', async () => {
  const f = fixture()
  await connected(f)
  await f.scenario.stop()
  await expect(f.scenario.dispatch('sample-selected-hr', {})).rejects.toMatchObject({
    code: 'scenario.no-selected-peer'
  })
  expect(f.characteristic.subscribe).not.toHaveBeenCalled()
})

test('late accepted subscription after timeout stays owned and cannot contaminate a new chooser run', async () => {
  const f = fixture()
  await connected(f)
  let accept
  f.characteristic.subscribe.mockImplementation(
    () =>
      new Promise(resolve => {
        accept = resolve
      })
  )
  await expect(f.scenario.dispatch('sample-selected-hr', { timeoutMs: 10 })).rejects.toMatchObject({
    code: 'scenario.notification-timeout'
  })
  await expect(f.scenario.dispatch('choose', {})).rejects.toMatchObject({ code: 'scenario.busy' })
  expect(f.host.createManager).toHaveBeenCalledTimes(1)
  accept(f.subscription)
  for (let turn = 0; turn < 20; turn += 1) await Promise.resolve()
  expect(f.subscription.remove).toHaveBeenCalledTimes(1)
  expect(f.iterator.next).not.toHaveBeenCalled()
  await f.scenario.dispatch('choose', {})
  expect(f.host.createManager).toHaveBeenCalledTimes(2)
  await expect(f.scenario.dispatch('sample-selected-hr', {})).rejects.toMatchObject({
    code: 'scenario.no-selected-peer'
  })
  await f.scenario.stop()
})

test('synchronously rejected subscription admission releases the pending gate for a fresh choice', async () => {
  const f = fixture()
  await connected(f)
  f.characteristic.subscribe.mockImplementation(() => {
    throw new Error('native admission refused')
  })
  await expect(f.scenario.dispatch('sample-selected-hr', { timeoutMs: 100 })).rejects.toThrow(
    'native admission refused'
  )
  await f.scenario.dispatch('choose', {})
  expect(f.host.createManager).toHaveBeenCalledTimes(2)
  await f.scenario.stop()
})

test('subscription time consumes the same monotonic sample deadline', async () => {
  const f = fixture()
  await connected(f)
  let monotonicMs = 100
  f.host.runtime.now = () => monotonicMs
  f.characteristic.subscribe.mockImplementation(async () => {
    monotonicMs = 130
    return f.subscription
  })
  f.deliver({
    done: false,
    value: {
      kind: 'value',
      value: { value: new Uint8Array([0, 72]), delivery: 'notification', sequence: 1, observedAtMonotonicMs: 1 }
    }
  })
  const timers = jest.spyOn(global, 'setTimeout')
  try {
    await f.scenario.dispatch('sample-selected-hr', { timeoutMs: 100 })
    expect(timers.mock.calls.map(call => call[1])).toEqual([100, 70])
  } finally {
    timers.mockRestore()
    await f.scenario.stop()
  }
})

test('refused subscription removal remains owned for an explicit stop retry', async () => {
  const f = fixture()
  await connected(f)
  f.subscription.remove.mockResolvedValue({ state: 'release-failed', failures: [] })
  f.deliver({
    done: false,
    value: {
      kind: 'value',
      value: { value: new Uint8Array([0, 72]), delivery: 'notification', sequence: 1, observedAtMonotonicMs: 1 }
    }
  })
  await expect(f.scenario.dispatch('sample-selected-hr', { timeoutMs: 100 })).rejects.toMatchObject({
    code: 'scenario.cleanup-failed'
  })
  f.subscription.remove.mockResolvedValue({ state: 'released', failures: [] })
  const cleanup = await f.scenario.stop()
  expect(cleanup.cleanup).toEqual(
    expect.arrayContaining([expect.objectContaining({ step: 'chooser.hrs.subscription.remove', state: 'released' })])
  )
})
