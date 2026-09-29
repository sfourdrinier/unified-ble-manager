const { createPublicBleManager, findPeerInScan } = require('../src/public/ble-manager')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
const { IpcPublicManagerAdapter } = require('../src/ipc/public-manager')

test.each([undefined, 5])('find rejects queued values consumed after its original %s deadline', async timeoutMs => {
  let now = 1000
  const source = new CoreBoundedStream(
    { itemCapacity: capacity(2), byteCapacity: capacity(1024), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  const stop = jest.fn(async () => ({ state: 'released', failures: [] }))
  const internal = {
    identity: null,
    attachedBackend: undefined,
    supports: () => true,
    scan: jest.fn(async () => ({ observations: source, stop })),
    destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
  const manager = await createPublicBleManager(internal, () => now)
  const pending = manager.find(timeoutMs === undefined ? {} : { timeoutMs })
  source.emit(
    { peerId: 'peer-1', localName: 'Sensor', rssi: -42, serviceUuids: [], manufacturerData: [], serviceData: [] },
    1
  )
  now += timeoutMs ?? 10000
  await expect(pending).rejects.toMatchObject({ code: 'operation.timed-out' })
  expect(stop).toHaveBeenCalledTimes(1)
})

test('a source rejection keeps its own identity rather than becoming a delivery timeout', async () => {
  const failure = new Error('native source refused')
  const scan = {
    observations: {
      [Symbol.asyncIterator]: () => ({
        next: async () => {
          throw failure
        }
      })
    }
  }
  await expect(findPeerInScan(scan, 'first', { signal: null, deadline: 1, now: () => 2 })).rejects.toBe(failure)
})

test.each([undefined, 5])('IPC find applies the same original %s delivery deadline', async timeoutMs => {
  jest.useFakeTimers({ doNotFake: ['nextTick', 'queueMicrotask', 'setImmediate', 'clearImmediate'] })
  try {
    const stop = jest.fn(async () => ({ state: 'released', failures: [] }))
    const host = {
      scan: async () => ({
        stop,
        observations: {
          async *[Symbol.asyncIterator]() {
            yield { kind: 'value', value: { peer: { id: 'late-peer' } } }
          }
        }
      })
    }
    const pending = IpcPublicManagerAdapter.prototype.find.call(host, timeoutMs === undefined ? {} : { timeoutMs })
    jest.advanceTimersByTime(timeoutMs ?? 10000)
    await expect(pending).rejects.toMatchObject({ code: 'operation.timed-out' })
    expect(stop).toHaveBeenCalledTimes(1)
  } finally {
    jest.useRealTimers()
  }
})

test('selection cannot return success after it requests abort', async () => {
  const controller = new AbortController()
  const scan = {
    observations: {
      async *[Symbol.asyncIterator]() {
        yield { kind: 'value', value: { peer: { id: 'peer' } } }
      }
    }
  }
  await expect(
    findPeerInScan(
      scan,
      () => {
        controller.abort()
        return true
      },
      { signal: controller.signal, deadline: null, now: () => 0 }
    )
  ).rejects.toMatchObject({ code: 'operation.aborted' })
})
