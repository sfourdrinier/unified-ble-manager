const pmd = require('../examples-shared/driver/polar-pmd')
const { main } = require('../scripts/native-protocol/test-h10-acc-radio')
const realTimeout = setTimeout
const realImmediate = setImmediate
beforeEach(() => jest.useFakeTimers({ doNotFake: ['setImmediate', 'clearImmediate'] }))
afterEach(() => {
  try {
    expect(jest.getTimerCount()).toBe(0)
  } finally {
    jest.useRealTimers()
  }
})

test('every probe scenario virtualizes watchdogs while retaining real event delivery', () => {
  expect(setTimeout).not.toBe(realTimeout)
  expect(setImmediate).toBe(realImmediate)
})

function stream() {
  const pending = []
  const queued = []
  let ended = false
  return {
    push(value) {
      const waiter = pending.shift()
      const item = { done: false, value: { kind: 'value', value: { value } } }
      if (waiter) waiter(item)
      else queued.push(item)
    },
    end() {
      ended = true
      for (const waiter of pending.splice(0)) waiter({ done: true })
    },
    [Symbol.asyncIterator]() {
      return this
    },
    next() {
      return queued.length
        ? Promise.resolve(queued.shift())
        : ended
          ? Promise.resolve({ done: true })
          : new Promise(resolve => {
              pending.push(resolve)
              if (this.onWaiting) setImmediate(this.onWaiting)
            })
    },
    async return() {
      this.end()
      return { done: true }
    }
  }
}

function harness({
  wrongName = false,
  failedStatus = false,
  cleanupFailure = false,
  malformed = false,
  wrongRate = false,
  unconfirmedWrite = false,
  stoppedOther = false
} = {}) {
  const cpStream = stream(),
    dataStream = stream(),
    logs = []
  const released = { state: 'released', failures: [] }
  let signalStall
  const stalled = new Promise(resolve => {
    signalStall = resolve
  })
  let acc = false,
    ecg = false,
    rate = 25,
    accSample = 0n,
    clock = 0n
  function frames() {
    for (let i = 0; i < 3; i++) {
      clock += 40000000n
      if (acc) {
        const bytes = new Uint8Array(16)
        bytes[0] = 2
        bytes[9] = 1
        new DataView(bytes.buffer).setInt16(10, 100, true)
        // Two frames with one sample must be spaced by exactly 1/rate.
        new DataView(bytes.buffer).setBigUint64(
          1,
          ++accSample * (1000000000n / BigInt(rate) + (wrongRate ? 1n : 0n)),
          true
        )
        dataStream.push(malformed ? bytes.slice(0, 15) : bytes)
      }
      if (ecg) {
        const bytes = new Uint8Array(13)
        bytes[9] = 0
        bytes[10] = 5
        new DataView(bytes.buffer).setBigUint64(1, clock, true)
        dataStream.push(bytes)
      }
    }
  }
  dataStream.onWaiting = frames
  const cp = {
    read: jest.fn(async () => Uint8Array.of(15, 5)),
    subscribe: jest.fn(async () => ({
      values: cpStream,
      remove: async () => {
        cpStream.end()
        return released
      }
    })),
    write: jest.fn(async bytes => {
      const [op, type] = bytes
      let status = failedStatus && op === 2 ? 8 : 0
      let parameters = []
      if (op === 1) parameters = [0, 4, 25, 0, 50, 0, 100, 0, 200, 0, 1, 1, 16, 0, 2, 3, 2, 0, 4, 0, 8, 0]
      if (op === 2 && type === 2 && status === 0) {
        acc = true
        accSample = 0n
        rate = bytes[4]
      }
      if (op === 2 && type === 0) ecg = true
      if (op === 3) {
        const losesOther = stoppedOther && ((type === 2 && ecg) || (type === 0 && acc))
        if (type === 2) acc = false
        else ecg = false
        if (stoppedOther) acc = ecg = false
        if (losesOther) signalStall()
      }
      cpStream.push(Uint8Array.from([240, op, type, status, 0, ...parameters]))
      frames()
      return { commitState: unconfirmedWrite ? 'unknown' : 'confirmed' }
    })
  }
  const data = {
    subscribe: jest.fn(async () => ({
      values: dataStream,
      remove: async () => {
        dataStream.end()
        return released
      }
    }))
  }
  const connection = {
    discover: async () => ({ characteristic: (_service, uuid) => (uuid === pmd.PMD_CONTROL_POINT ? cp : data) }),
    disconnect: jest.fn(async () => released)
  }
  const manager = {
    find: jest.fn(async options => {
      const peer = { name: wrongName ? 'real H10' : 'SIM Polar H10 0001' }
      if (!options.select(peer)) throw new Error('exact simulator not found')
      return peer
    }),
    connect: jest.fn(async () => connection),
    destroy: jest.fn(async () => (cleanupFailure ? { state: 'release-failed', failures: [{}] } : released))
  }
  return {
    manager,
    connection,
    cp,
    dataStream,
    logs,
    stalled,
    options: {
      pmd,
      createManager: async () => manager,
      env: { UBM_NAPI_ADDON: '/tmp/fixture.node', UBM_RADIO_PLATFORM: 'bluez' },
      timeoutMs: 20,
      log: entry => logs.push(entry)
    }
  }
}

test('ACC probe exercises all twelve settings plus both interleaved stop orders through public UBM APIs', async () => {
  const run = harness()
  await main(run.options)
  expect(run.logs.filter(entry => entry.phase === 'acc-setting')).toHaveLength(12)
  expect(run.logs.at(-1).phase).toBe('passed')
  expect(run.manager.destroy).toHaveBeenCalledTimes(1)
  expect(run.connection.disconnect).toHaveBeenCalledTimes(1)
})

test('fixture ACC sample time remains monotonic across generated batches', async () => {
  const run = harness()
  await run.cp.write(pmd.buildStartAccCommand({ sampleRateHz: 25, resolutionBits: 16, rangeG: 2 }))
  const timestamps = []
  for (let index = 0; index < 6; index++) {
    const next = await run.dataStream.next()
    timestamps.push(pmd.parseAccFrame(next.value.value.value).timestampNs)
  }
  expect(timestamps).toEqual([40000000n, 80000000n, 120000000n, 160000000n, 200000000n, 240000000n])
  await run.dataStream.return()
})

test.each([
  [{ wrongName: true }, 'exact simulator'],
  [{ failedStatus: true }, 'PMD command refused'],
  [{ malformed: true }, 'non-zero multiple'],
  [{ wrongRate: true }, 'timestamp/sample-rate mismatch'],
  [{ unconfirmedWrite: true }, 'write was not confirmed'],
  [{ cleanupFailure: true }, 'cleanup remained unresolved']
])('ACC probe fails closed and always destroys its manager: %j', async (fault, message) => {
  const run = harness(fault)
  await expect(main(run.options)).rejects.toThrow(message)
  expect(run.manager.destroy).toHaveBeenCalledTimes(1)
  expect(run.logs.some(entry => entry.phase === 'passed')).toBe(false)
})

test('stopping one stream must not silently stop the other (virtual failure watchdog)', async () => {
  const run = harness({ stoppedOther: true })
  const result = expect(main(run.options)).rejects.toThrow('timed out')
  await run.stalled
  // Flush the actual stream-delivery turn; no elapsed duration establishes
  // completion. The intentionally absent stream can fail only on virtual time.
  await new Promise(resolve => setImmediate(resolve))
  expect(jest.getTimerCount()).toBeGreaterThan(0)
  await jest.advanceTimersByTimeAsync(run.options.timeoutMs)
  await result
  expect(run.manager.destroy).toHaveBeenCalledTimes(1)
  expect(run.logs.some(entry => entry.phase === 'passed')).toBe(false)
})
