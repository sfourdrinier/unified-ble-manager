'use strict'

const { awaitSignal } = require('../helpers/async')
const { IpcPublicManagerAdapter } = require('../../src/ipc/public-manager')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { capacity } = require('../../src/backend-contract/primitives')

function descriptor(id, state = 'unsupported', limitations = []) {
  return { id, state, limitations }
}

function capabilities() {
  const descriptors = new Map([
    ['connection:direct', descriptor('connection:direct', 'supported')],
    ['discovery:continuous-scan', descriptor('discovery:continuous-scan', 'supported')]
  ])
  return {
    supports: id => descriptors.get(id)?.state === 'supported',
    get: id => descriptors.get(id),
    require: id => descriptors.get(id) ?? descriptor(id),
    list: () => [...descriptors.values()]
  }
}

const provisionalPath = {
  attachment: { backendInstanceId: 'backend-1', backendGeneration: 'generation-1' },
  attachmentId: 'attachment-1',
  peerId: 'peer-1',
  connectionId: 'connection-1',
  connectionGeneration: 'generation-1',
  ownerLeaseId: 'lease-1',
  databaseId: 'database-1',
  databaseGeneration: 'generation-1',
  serviceUuid: '180f',
  serviceOccurrence: '0',
  characteristicUuid: '2a19',
  characteristicOccurrence: '0',
  validity: 'current'
}

function createIpcScanFixture() {
  const observations = new CoreBoundedStream(
    { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  const stop = jest.fn(async () => ({ state: 'released', failures: [] }))
  const capabilitySnapshot = capabilities()
  const ipc = {
    capabilities: capabilitySnapshot,
    bootstrap: { discovery: { kind: 'continuous-scan' } },
    scan: async () => ({
      plan: null,
      observations,
      stop
    }),
    destroy: jest.fn(async () => ({ state: 'released', failures: [] })),
    adapterState: async () => ({})
  }
  const manager = new IpcPublicManagerAdapter(ipc, {
    capabilities: capabilitySnapshot,
    adapter: { id: 'adapter-1', state: async () => ({}), waitUntilReady: async () => ({}) }
  })
  return { manager, observations, stop, ipc }
}

describe('IPC public manager teardown', () => {
  test('a late child rejection remains diagnostic when the parent repeatedly refuses release', async () => {
    const { manager, ipc } = createIpcScanFixture()
    const lateError = new Error('late-child-remove-failed')
    let rejectChild
    const child = new Promise((_, reject) => {
      rejectChild = reject
    })
    const primaryError = new Error('projection-failed')
    let removals = 0
    const compensation = manager.provisionalSubscriptions
      .compensate(
        {
          remove: () => {
            removals += 1
            return removals === 1 ? child : Promise.resolve({ state: 'released', failures: [] })
          }
        },
        provisionalPath,
        primaryError
      )
      .catch(error => error)
    let releaseAttempts = 0
    ipc.destroy.mockImplementation(async () => {
      releaseAttempts += 1
      return releaseAttempts < 3
        ? {
            state: 'release-failed',
            failures: [
              {
                resourceKind: 'renderer-lease',
                error: {
                  code: 'platform.transport',
                  domain: 'ipc',
                  operation: 'fixture.release',
                  platform: null,
                  retryability: 'caller-decides'
                }
              }
            ]
          }
        : { state: 'released', failures: [] }
    })
    await expect(manager.destroy()).rejects.toMatchObject({ name: 'AggregateError' })
    rejectChild(lateError)
    const compensationError = await compensation
    expect(compensationError.errors).toContain(primaryError)
    await expect(manager.destroy()).rejects.toMatchObject({ errors: expect.arrayContaining([lateError]) })
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })
  test('a pending provisional subscription cannot prevent authoritative lease release', async () => {
    const { manager, ipc } = createIpcScanFixture()
    let noteRelease
    const releaseReached = new Promise(resolve => {
      noteRelease = resolve
    })
    ipc.destroy.mockImplementation(async () => {
      noteRelease()
      return { state: 'released', failures: [] }
    })
    let rejectChild
    const child = new Promise((_, reject) => {
      rejectChild = reject
    })
    const primaryError = new Error('projection-failed')
    const compensation = manager.provisionalSubscriptions
      .compensate({ remove: () => child }, provisionalPath, primaryError)
      .catch(error => error)
    const result = manager.destroy()
    await awaitSignal(releaseReached, 'IPC public parent lease release despite unresolved child removal')
    expect(ipc.destroy).toHaveBeenCalledTimes(1)
    await expect(result).resolves.toEqual({ state: 'released', failures: [] })
    const compensationError = await compensation
    expect(compensationError).toBe(primaryError)
    rejectChild(new Error('late-child-remove-failed'))
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })
})

async function collectRemainingScanStates(iterator) {
  const states = []
  for (;;) {
    const next = await iterator.next()
    if (next.done) return states
    states.push(next.value)
  }
}

describe('IPC public scan session state', () => {
  test('released parent retires an active scan without dispatching a child stop', async () => {
    const { manager, stop } = createIpcScanFixture()
    const scan = await manager.scan()
    const states = scan.state[Symbol.asyncIterator]()
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'active' } })
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopped' } })
    await expect(states.next()).resolves.toMatchObject({ done: true })
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(stop).not.toHaveBeenCalled()
  })

  test.each(['resolve', 'reject'])(
    'released parent settles an in-flight stop and ignores its late %s',
    async lateOutcome => {
      const { manager, stop, ipc } = createIpcScanFixture()
      let settleStop
      const stopGate = new Promise((resolve, reject) => {
        settleStop = lateOutcome === 'resolve' ? resolve : reject
      })
      stop.mockImplementation(() => stopGate)
      const scan = await manager.scan()
      const states = scan.state[Symbol.asyncIterator]()
      await expect(states.next()).resolves.toMatchObject({ value: { state: 'active' } })
      const stopping = scan.stop()
      await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopping' } })
      await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(stopping).resolves.toEqual({ state: 'released', failures: [] })
      await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopped' } })
      await expect(states.next()).resolves.toMatchObject({ done: true })
      expect(stop).toHaveBeenCalledTimes(1)
      expect(ipc.destroy).toHaveBeenCalledTimes(1)
      settleStop(lateOutcome === 'resolve' ? { state: 'released', failures: [] } : new Error('late IPC stop'))
      await Promise.resolve()
      expect(stop).toHaveBeenCalledTimes(1)
    }
  )

  test('refused parent release leaves a pending child stop owned for its eventual result', async () => {
    const { manager, stop, ipc } = createIpcScanFixture()
    let finishStop
    const stopGate = new Promise(resolve => {
      finishStop = resolve
    })
    stop.mockImplementation(() => stopGate)
    ipc.destroy
      .mockResolvedValueOnce({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'renderer-lease',
            error: {
              code: 'platform.transport',
              domain: 'ipc',
              operation: 'fixture.release',
              platform: null,
              retryability: 'caller-decides'
            }
          }
        ]
      })
      .mockResolvedValueOnce({ state: 'released', failures: [] })
    const scan = await manager.scan()
    const states = scan.state[Symbol.asyncIterator]()
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'active' } })
    const stopping = scan.stop()
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopping' } })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expect(stop).toHaveBeenCalledTimes(1)
    finishStop({ state: 'released', failures: [] })
    await expect(stopping).resolves.toEqual({ state: 'released', failures: [] })
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'stopped' } })
    await expect(states.next()).resolves.toMatchObject({ done: true })
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })
  test.each([
    ['source-failed', { state: 'failed', reason: 'source-failed' }],
    ['connection-lost', { state: 'failed', reason: 'connection-lost' }],
    ['overflow', { state: 'failed', reason: 'overflow' }],
    ['closed', { state: 'stopped', reason: 'closed' }]
  ])('%s without stop() leaves the session not active with no iterator', async (reason, expected) => {
    const { manager, observations, stop } = createIpcScanFixture()
    const scan = await manager.scan()
    const states = scan.state[Symbol.asyncIterator]()
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'active' } })
    observations.finishWithReason(reason)
    const terminal = await awaitSignal(states.next(), `${reason} to terminalize IPC session state`)
    expect(terminal.value.state).not.toBe('active')
    expect(terminal.value).toMatchObject(expected)
    expect(stop).not.toHaveBeenCalled()
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(stop).toHaveBeenCalledTimes(1)
    const rest = await awaitSignal(collectRemainingScanStates(states), 'IPC scan state stream to close after stop')
    expect(rest.every(event => event.state !== 'active')).toBe(true)
  })

  test.each([
    ['source-failed', { state: 'failed', reason: 'source-failed' }],
    ['connection-lost', { state: 'failed', reason: 'connection-lost' }],
    ['overflow', { state: 'failed', reason: 'overflow' }],
    ['closed', { state: 'stopped', reason: 'closed' }]
  ])('%s without stop() leaves the session not active with an attached iterator', async (reason, expected) => {
    const { manager, observations, stop } = createIpcScanFixture()
    const scan = await manager.scan()
    const states = scan.state[Symbol.asyncIterator]()
    const iterator = scan.observations[Symbol.asyncIterator]()
    await expect(states.next()).resolves.toMatchObject({ value: { state: 'active' } })
    const observationTerminal = iterator.next()
    observations.finishWithReason(reason)
    await expect(observationTerminal).resolves.toMatchObject({ value: { kind: 'terminal', reason } })
    const terminal = await awaitSignal(states.next(), `${reason} to terminalize IPC session state`)
    expect(terminal.value.state).not.toBe('active')
    expect(terminal.value).toMatchObject(expected)
    expect(stop).not.toHaveBeenCalled()
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test.each([
    ['source-failed', { state: 'failed', reason: 'source-failed' }],
    ['connection-lost', { state: 'failed', reason: 'connection-lost' }],
    ['overflow', { state: 'failed', reason: 'overflow' }],
    ['closed', { state: 'stopped', reason: 'closed' }]
  ])('%s that raced scan.start is the initial state and never publishes active', async (reason, expected) => {
    const { manager, observations, stop } = createIpcScanFixture()
    observations.finishWithReason(reason)
    const scan = await manager.scan()
    const states = scan.state[Symbol.asyncIterator]()
    const first = await awaitSignal(states.next(), `already-terminal ${reason} as initial IPC session state`)
    expect(first.value.state).not.toBe('active')
    expect(first.value).toMatchObject(expected)
    expect(stop).not.toHaveBeenCalled()
    await expect(scan.observations[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason }
    })
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(stop).toHaveBeenCalledTimes(1)
    const rest = await awaitSignal(collectRemainingScanStates(states), 'IPC scan state stream to close after stop')
    expect([first.value, ...rest].some(event => event.state === 'active')).toBe(false)
  })
})
