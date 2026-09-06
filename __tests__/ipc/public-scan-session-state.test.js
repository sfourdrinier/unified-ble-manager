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
    adapterState: async () => ({})
  }
  const manager = new IpcPublicManagerAdapter(ipc, {
    capabilities: capabilitySnapshot,
    adapter: { id: 'adapter-1', state: async () => ({}), waitUntilReady: async () => ({}) }
  })
  return { manager, observations, stop }
}

async function collectRemainingScanStates(iterator) {
  const states = []
  for (;;) {
    const next = await iterator.next()
    if (next.done) return states
    states.push(next.value)
  }
}

describe('IPC public scan session state', () => {
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
  ])(
    '%s that raced scan.start is the initial state and never publishes active',
    async (reason, expected) => {
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
    }
  )
})
