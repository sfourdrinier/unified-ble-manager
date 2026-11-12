const { installWebRecordingAuthorityWorker } = require('../../src/web/web-recording-authority-worker')
const { probeWebRecordingSharedWorker, acquireWebRecordingWorkerLock } = require('../../src/web/web-recording-authority-worker-client')

const url = 'https://example.test/assets/../worker.js'
let ports
let constructorCalls
let originalWorker

beforeEach(() => {
  originalWorker = global.SharedWorker
  ports = []
  constructorCalls = []
  global.SharedWorker = class {
    constructor(workerUrl, options) {
      constructorCalls.push([workerUrl, options])
      this.onerror = null
      this.port = {
        onmessage: null, onmessageerror: null, onclose: null,
        postMessage: jest.fn(), start: jest.fn(), close: jest.fn()
      }
      ports.push(this.port)
    }
  }
})

afterEach(() => {
  global.SharedWorker = originalWorker
  jest.useRealTimers()
})

const hello = port => port.onmessage({ data: { type: 'hello', version: 2, requestId: port.postMessage.mock.calls[0][0].requestId } })

test('constructs the fixed module identity and closes the successful probe', async () => {
  const pending = probeWebRecordingSharedWorker(url)
  expect(constructorCalls).toEqual([['https://example.test/worker.js', { name: 'ubm-ordered-recording-v2', type: 'module' }]])
  expect(ports[0].postMessage.mock.calls[0][0]).toMatchObject({ type: 'hello', version: 2 })
  hello(ports[0])
  await expect(pending).resolves.toEqual({ kind: 'worker', url: 'https://example.test/worker.js', name: 'ubm-ordered-recording-v2', type: 'module', version: 2 })
  expect(ports[0].close).toHaveBeenCalledTimes(1)
})

test('worker responds only to compatible HELLO without admitting or recording an owner', () => {
  const scope = { onconnect: null }
  installWebRecordingAuthorityWorker(scope)
  const connect = message => {
    const port = { onmessage: null, onmessageerror: null, start: jest.fn(), close: jest.fn(), postMessage: jest.fn() }
    scope.onconnect({ ports: [port] })
    port.onmessage({ data: message })
    return port
  }
  const request = { type: 'hello', version: 2, requestId: '1' }
  expect(connect(request).postMessage).toHaveBeenCalledWith(request)
  expect(connect(request).postMessage).toHaveBeenCalledWith(request)
  const oldAcquire = connect({ type: 'acquire', name: `ubm-ordered-recording:${'12'.repeat(32)}` })
  expect(oldAcquire.postMessage).not.toHaveBeenCalled()
  expect(oldAcquire.close).toHaveBeenCalledTimes(1)
})

test('HELLO response is delivered over an actual MessageChannel before the worker closes its port', async () => {
  const { MessageChannel } = require('worker_threads')
  const scope = { onconnect: null }
  installWebRecordingAuthorityWorker(scope)
  const { port1, port2 } = new MessageChannel()
  scope.onconnect({ ports: [port2] })
  const reply = new Promise(resolve => { port1.onmessage = event => resolve(event.data) })
  port1.postMessage({ type: 'hello', version: 2, requestId: '42' })
  await expect(reply).resolves.toEqual({ type: 'hello', version: 2, requestId: '42' })
  port1.close()
})

test('worker bounds and times out ports that never send HELLO', () => {
  jest.useFakeTimers()
  const scope = { onconnect: null }
  installWebRecordingAuthorityWorker(scope)
  const ports = Array.from({ length: 17 }, () => ({ onmessage: null, onmessageerror: null, start: jest.fn(), close: jest.fn(), postMessage: jest.fn() }))
  for (const port of ports) scope.onconnect({ ports: [port] })
  expect(ports[16].close).toHaveBeenCalledTimes(1)
  jest.advanceTimersByTime(5000)
  expect(ports.every(port => port.close.mock.calls.length === 1)).toBe(true)
  const next = { onmessage: null, onmessageerror: null, start: jest.fn(), close: jest.fn(), postMessage: jest.fn() }
  scope.onconnect({ ports: [next] })
  next.onmessage({ data: { type: 'hello', version: 2, requestId: '1' } })
  expect(next.postMessage).toHaveBeenCalledTimes(1)
  expect(next.close).toHaveBeenCalledTimes(1)
  jest.useRealTimers()
})

test('silent, cancelled and late replies close pre-admission ports', async () => {
  jest.useFakeTimers()
  const timed = probeWebRecordingSharedWorker(url)
  const rejected = expect(timed).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  const late = ports[0].onmessage
  const request = ports[0].postMessage.mock.calls[0][0]
  await jest.advanceTimersByTimeAsync(5000)
  await rejected
  expect(ports[0].close).toHaveBeenCalledTimes(1)
  late({ data: request })
  expect(ports[0].close).toHaveBeenCalledTimes(1)
  const controller = new AbortController()
  const cancelled = probeWebRecordingSharedWorker(url, controller.signal)
  controller.abort()
  await expect(cancelled).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  expect(ports[1].close).toHaveBeenCalledTimes(1)
  jest.useRealTimers()
})

test('rejects mismatched or malformed replies and invalid URL without admitting', async () => {
  for (const data of [{ type: 'hello', version: 1, requestId: '1' }, { type: 'hello', version: 2, requestId: 'stale' }, { state: 'acquired' }]) {
    const pending = probeWebRecordingSharedWorker(url)
    const port = ports.at(-1)
    port.onmessage({ data })
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
    expect(port.close).toHaveBeenCalledTimes(1)
  }
  await expect(probeWebRecordingSharedWorker('./worker.js')).rejects.toMatchObject({ normalized: { code: 'argument.invalid' } })
})

test('pre-abort, construction failure and message error fail before admission', async () => {
  const controller = new AbortController()
  controller.abort()
  await expect(probeWebRecordingSharedWorker(url, controller.signal)).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  expect(ports).toHaveLength(0)
  global.SharedWorker = class { constructor() { throw new Error('denied') } }
  await expect(probeWebRecordingSharedWorker(url)).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  global.SharedWorker = class {
    constructor() {
      this.onerror = null
      this.port = { onmessage: null, onmessageerror: null, postMessage() {}, start() {}, close: jest.fn() }
      ports.push(this.port)
    }
  }
  const pending = probeWebRecordingSharedWorker(url)
  ports[0].onmessageerror()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(ports[0].close).toHaveBeenCalledTimes(1)
})

test('a cancelled attempt cannot satisfy a later probe even when its reply arrives late', async () => {
  const controller = new AbortController()
  const first = probeWebRecordingSharedWorker(url, controller.signal)
  const lateHandler = ports[0].onmessage
  const oldRequest = ports[0].postMessage.mock.calls[0][0]
  controller.abort()
  await expect(first).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  const second = probeWebRecordingSharedWorker(url)
  expect(ports[1].postMessage.mock.calls[0][0].requestId).not.toBe(oldRequest.requestId)
  lateHandler({ data: oldRequest })
  ports[1].onmessage({ data: oldRequest })
  await expect(second).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(ports[1].close).toHaveBeenCalledTimes(1)
})

test('bounded concurrent attempts reject excess and repeated timeouts retire all ports', async () => {
  jest.useFakeTimers()
  const attempts = Array.from({ length: 16 }, () => probeWebRecordingSharedWorker(url))
  const rejected = attempts.map(attempt => expect(attempt).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } }))
  await expect(probeWebRecordingSharedWorker(url)).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
  expect(ports).toHaveLength(16)
  await jest.advanceTimersByTimeAsync(5000)
  await Promise.all(rejected)
  expect(ports.every(port => port.close.mock.calls.length === 1)).toBe(true)
  const next = probeWebRecordingSharedWorker('https://example.test/other.js')
  expect(constructorCalls.at(-1)).toEqual(['https://example.test/other.js', { name: 'ubm-ordered-recording-v2', type: 'module' }])
  hello(ports.at(-1))
  await next
  jest.useRealTimers()
})

test('legacy worker acquisition fails closed until durable reservations are integrated', async () => {
  const factory = jest.fn()
  await expect(acquireWebRecordingWorkerLock(factory, async () => '12'.repeat(32), 'id')).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(factory).not.toHaveBeenCalled()
})
