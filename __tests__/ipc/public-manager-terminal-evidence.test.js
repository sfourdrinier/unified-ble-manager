'use strict'

const { IpcPublicManagerAdapter } = require('../../src/ipc/public-manager')
const { emptyIpcDiagnostics } = require('../helpers/ipc-diagnostics')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { capacity } = require('../../src/backend-contract/primitives')

function capabilities() {
  const descriptors = new Map([
    ['connection:direct', { id: 'connection:direct', state: 'supported', limitations: [] }],
    ['discovery:continuous-scan', { id: 'discovery:continuous-scan', state: 'supported', limitations: [] }]
  ])
  return {
    supports: id => descriptors.get(id)?.state === 'supported',
    get: id => descriptors.get(id),
    require: id => descriptors.get(id),
    list: () => [...descriptors.values()]
  }
}

function createManager({ scan, reserveTerminalEvidence, connect, destroy = async () => ({ state: 'released', failures: [] }) }) {
  const capabilitySnapshot = capabilities()
  const ipc = {
    capabilities: capabilitySnapshot,
    bootstrap: { attachment: { attachmentId: 'attachment-1' }, discovery: { kind: 'continuous-scan' } },
    diagnostics: emptyIpcDiagnostics,
    scan,
    reserveTerminalEvidence,
    connect,
    destroy,
    adapterState: async () => ({})
  }
  return new IpcPublicManagerAdapter(ipc, {
    capabilities: capabilitySnapshot,
    adapter: { id: 'adapter-1', state: async () => ({}), waitUntilReady: async () => ({}) },
    terminalEvidence: { maximumCanonicalBytes: 512 }
  })
}

function emptyEvents() {
  return { [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }) }) }
}

function subscriptionDatabase(subscribe) {
  const path = {
    attachment: {}, attachmentId: 'attachment-1', peerId: 'peer-1', connectionId: 'connection-1', ownerLeaseId: 'lease-1',
    connectionGeneration: 'connection-generation-1', databaseId: 'database-1', databaseGeneration: 'database-generation-1'
  }
  return {
    path,
    assertCurrent: () => undefined,
    monotonicNow: () => 1,
    scheduleDeadline: (_deadline, action) => action(),
    snapshot: async () => ({
      path,
      services: [{ path: { ...path, serviceUuid: '180f', serviceOccurrence: '0' } }],
      characteristics: [{
        path: { ...path, serviceUuid: '180f', serviceOccurrence: '0', characteristicUuid: '2a19', characteristicOccurrence: '0', validity: 'current' },
        properties: { read: false, writeWithResponse: false, writeWithoutResponse: false, notify: true }
      }],
      descriptors: []
    }),
    subscribe
  }
}

function connection(database) {
  return {
    handle: 'connection-handle-1', peerId: 'peer-1', attachmentId: 'attachment-1', connectionId: 'connection-1',
    ownerLeaseId: 'lease-1', connectionGeneration: 'connection-generation-1', events: emptyEvents(),
    discover: async () => database,
    disconnect: async () => ({ state: 'released', failures: [] }),
    release: async () => ({ state: 'released', failures: [] })
  }
}

function reservation(release) {
  return { maximumBytes: 512, commit: async () => ({ kind: 'refused', reason: 'storage-unavailable' }), release }
}

describe('IPC public-manager terminal evidence admission cleanup', () => {
  test('releases a scan terminal reservation when scan admission rejects', async () => {
    const admissionError = new Error('scan admission rejected')
    const release = jest.fn(async () => undefined)
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(release),
      scan: async () => {
        throw admissionError
      }
    })

    await expect(manager.scan()).rejects.toBe(admissionError)
    expect(release).toHaveBeenCalledTimes(1)
  })

  test('preserves cancellation admission failure while releasing its reservation', async () => {
    const cancellation = new Error('scan cancelled')
    const release = jest.fn(async () => undefined)
    const controller = new AbortController()
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(release),
      scan: async () => {
        controller.abort()
        throw cancellation
      }
    })

    await expect(manager.scan({ signal: controller.signal })).rejects.toBe(cancellation)
    expect(release).toHaveBeenCalledTimes(1)
  })

  test('releases a subscription terminal reservation when subscription admission rejects', async () => {
    const admissionError = new Error('subscription admission rejected')
    const release = jest.fn(async () => undefined)
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(release),
      connect: async () => connection(subscriptionDatabase(async () => { throw admissionError }))
    })

    const database = await (await manager.connect('peer-1')).discover()
    const characteristic = database.service('180f').characteristic('2a19')
    await expect(characteristic.subscribe()).rejects.toBe(admissionError)
    expect(release).toHaveBeenCalledTimes(1)
  })

  test('releases each concurrently rejected scan reservation exactly once', async () => {
    const releases = [jest.fn(async () => undefined), jest.fn(async () => undefined)]
    let reservationIndex = 0
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(releases[reservationIndex++]),
      scan: async () => {
        throw new Error('scan admission rejected')
      }
    })

    await expect(Promise.allSettled([manager.scan(), manager.scan()])).resolves.toEqual([
      expect.objectContaining({ status: 'rejected' }),
      expect.objectContaining({ status: 'rejected' })
    ])
    expect(releases[0]).toHaveBeenCalledTimes(1)
    expect(releases[1]).toHaveBeenCalledTimes(1)
  })

  test('preserves both admission and terminal cleanup failure, then retries cleanup during destroy', async () => {
    const admissionError = new Error('scan admission rejected')
    const cleanupError = new Error('terminal reservation release rejected')
    const release = jest
      .fn()
      .mockRejectedValueOnce(cleanupError)
      .mockResolvedValueOnce(undefined)
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(release),
      scan: async () => {
        throw admissionError
      }
    })

    const error = await manager.scan().catch(value => value)
    expect(error).toMatchObject({ name: 'AggregateError' })
    expect(error.errors).toEqual(expect.arrayContaining([admissionError, cleanupError]))
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(release).toHaveBeenCalledTimes(2)
  })

  test('destroy does not release an active scan reservation before source teardown', async () => {
    const release = jest.fn(async () => undefined)
    const observations = new CoreBoundedStream(
      { itemCapacity: capacity(8), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(release),
      scan: async () => ({ plan: null, observations, stop: async () => ({ state: 'released', failures: [] }) })
    })

    await manager.scan()
    expect(release).not.toHaveBeenCalled()
    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(release).not.toHaveBeenCalled()
  })

  test('concurrent destroy retries share one retained terminal cleanup attempt', async () => {
    const admissionError = new Error('scan admission rejected')
    const cleanupError = new Error('terminal reservation release rejected')
    let finishRetry
    const retry = new Promise(resolve => {
      finishRetry = resolve
    })
    const release = jest
      .fn()
      .mockRejectedValueOnce(cleanupError)
      .mockImplementationOnce(() => retry)
    const manager = createManager({
      reserveTerminalEvidence: async () => reservation(release),
      scan: async () => {
        throw admissionError
      }
    })

    await expect(manager.scan()).rejects.toMatchObject({ name: 'AggregateError' })
    const first = manager.destroy()
    const second = manager.destroy()
    expect(release).toHaveBeenCalledTimes(2)
    finishRetry()
    await expect(first).resolves.toEqual({ state: 'released', failures: [] })
    await expect(second).resolves.toEqual({ state: 'released', failures: [] })
    expect(release).toHaveBeenCalledTimes(2)
  })

  test('same-time module reloads mint distinct terminal-evidence scopes', async () => {
    const originalCrypto = globalThis.crypto
    const now = jest.spyOn(Date, 'now').mockReturnValue(1)
    let entropy = 0
    Object.defineProperty(globalThis, 'crypto', {
      configurable: true,
      value: { getRandomValues: bytes => { bytes.fill(++entropy); return bytes } }
    })
    const scopes = []
    const rejectedScan = async () => {
      throw new Error('scan admission rejected')
    }
    try {
      const loadFreshManager = () => {
        jest.resetModules()
        const { IpcPublicManagerAdapter: ReloadedIpcPublicManagerAdapter } = require('../../src/ipc/public-manager')
        const capabilitySnapshot = capabilities()
        return new ReloadedIpcPublicManagerAdapter(
          {
            capabilities: capabilitySnapshot,
            bootstrap: { attachment: { attachmentId: 'attachment-1' }, discovery: { kind: 'continuous-scan' } },
            diagnostics: emptyIpcDiagnostics,
            reserveTerminalEvidence: async request => {
              scopes.push(request.scope)
              return reservation(async () => undefined)
            },
            scan: rejectedScan,
            adapterState: async () => ({})
          },
          {
            capabilities: capabilitySnapshot,
            adapter: { id: 'adapter-1', state: async () => ({}), waitUntilReady: async () => ({}) },
            terminalEvidence: { maximumCanonicalBytes: 512 }
          }
        )
      }
      const first = loadFreshManager()
      await expect(first.scan()).rejects.toThrow('scan admission rejected')
      const reloaded = loadFreshManager()
      await expect(reloaded.scan()).rejects.toThrow('scan admission rejected')
      expect(scopes).toHaveLength(2)
      expect(scopes[1]).not.toEqual(scopes[0])
    } finally {
      now.mockRestore()
      Object.defineProperty(globalThis, 'crypto', { configurable: true, value: originalCrypto })
    }
  })
})
