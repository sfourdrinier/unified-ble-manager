const { createPublicBleManager } = require('../src/public/ble-manager')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')

async function controls(values) {
  const measured = { connectionId: 'c', connectionGeneration: 'g', observedAtMonotonicMs: 100, ordinal: 1, ...values }
  const source = new CoreBoundedStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
  source.emit(measured, 128)
  const close = jest.fn(async () => {
    source.close()
    return { state: 'released', failures: [] }
  })
  const connection = {
    connectionId: 'c',
    connectionGeneration: 'g',
    events: { async *[Symbol.asyncIterator]() {} },
    parameters: async () => measured,
    parameterEvents: async () => ({ events: source, close })
  }
  const supported = new Set(['connection:direct', 'connection:parameters'])
  const internal = {
    supports: id => supported.has(id),
    capability: id => (supported.has(id) ? { state: 'supported', limitations: [] } : null),
    capabilities: () => [],
    connect: async () => connection
  }
  const manager = await createPublicBleManager(internal, () => 100, { peerId: value => value })
  return { controls: (await manager.connect('p')).controls, close }
}

test.each([
  { intervalUs: 0 },
  { intervalUs: NaN },
  { intervalUs: Infinity },
  { intervalUs: -1 },
  { intervalUs: 1.5 },
  { intervalUs: Number.MAX_SAFE_INTEGER + 1 },
  { latency: -1 },
  { latency: 0.5 },
  { latency: Infinity },
  { supervisionTimeoutUs: 0 },
  { supervisionTimeoutUs: NaN },
  { supervisionTimeoutUs: Infinity },
  { supervisionTimeoutUs: 1.5 },
  { supervisionTimeoutUs: Number.MAX_SAFE_INTEGER + 1 }
])('snapshot and stream both reject malformed parameter values: %j', async invalid => {
  const { controls: control, close } = await controls({
    intervalUs: 30_000,
    latency: 2,
    supervisionTimeoutUs: 4_000_000,
    ...invalid
  })
  await expect(control.parameters()).rejects.toMatchObject({ code: 'protocol.violation' })
  const iterator = control.parameterEvents()[Symbol.asyncIterator]()
  await expect(iterator.next()).rejects.toMatchObject({ code: 'protocol.violation' })
  expect(close).toHaveBeenCalledTimes(1)
})

test('snapshot and stream agree on valid parameter units and identity', async () => {
  const { controls: control } = await controls({ intervalUs: 30_000, latency: 2, supervisionTimeoutUs: 4_000_000 })
  const snapshot = await control.parameters()
  const iterator = control.parameterEvents()[Symbol.asyncIterator]()
  const streamed = await iterator.next()
  expect(streamed.value).toEqual(snapshot)
  expect(snapshot).toMatchObject({
    intervalMs: 30,
    peripheralLatency: 2,
    supervisionTimeoutMs: 4000,
    connectionGeneration: 'g'
  })
  await iterator.return()
})


test.each([1001, Number.MAX_SAFE_INTEGER])('positive safe-integer microseconds %s remain measurable without imposing a native-only ceiling', async micros => {
  const { controls: control } = await controls({ intervalUs: micros, latency: 0, supervisionTimeoutUs: micros })
  const snapshot = await control.parameters()
  const iterator = control.parameterEvents()[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toEqual(snapshot)
  expect(snapshot.intervalMs).toBe(micros / 1000)
  expect(snapshot.supervisionTimeoutMs).toBe(micros / 1000)
  await iterator.return()
})
