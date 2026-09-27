const { createPublicPeerDirectory } = require('../src/public/peer-directory')
const { contractError } = require('../src/backend-contract/errors')

const reference = { version: 1, backendId: 'react-native-android', scope: 'origin', opaqueId: 'peer' }
const methods = ['resolve', 'known', 'connected', 'bonded', 'authorized', 'restored']
const call = (directory, method, options) =>
  method === 'resolve' ? directory.resolve(reference, options) : directory[method](options)
const empty = method => (method === 'resolve' ? null : [])
function fixture(method, implementation, now = () => Date.now()) {
  const backend = Object.fromEntries(
    methods.map(name => [name, jest.fn(implementation ?? (() => Promise.resolve(empty(name))))])
  )
  return {
    backend,
    invoke: options => call(createPublicPeerDirectory(backend, now), method, options)
  }
}

beforeEach(() => {
  jest.useFakeTimers({ doNotFake: ['setImmediate'] })
  jest.setSystemTime(1000)
})
afterEach(() => {
  expect(jest.getTimerCount()).toBe(0)
  jest.useRealTimers()
})

describe.each(methods)('%s public directory admission', method => {
  test('expired pre-dispatch deadline never calls the backend', async () => {
    const now = jest.fn().mockReturnValueOnce(1000).mockReturnValue(1010)
    const { backend, invoke } = fixture(method, undefined, now)
    await expect(invoke({ timeoutMs: 10 })).rejects.toMatchObject({ code: 'operation.timed-out' })
    expect(backend[method]).not.toHaveBeenCalled()
  })
  test('late success cannot win while the deadline timer is suspended', async () => {
    let resolveNative
    let now = 1000
    const { invoke } = fixture(
      method,
      () =>
        new Promise(resolve => {
          resolveNative = resolve
        }),
      () => now
    )
    const result = invoke({ timeoutMs: 10 })
    now = 1011
    resolveNative(empty(method))
    await expect(result).rejects.toMatchObject({ code: 'operation.timed-out' })
  })
  test('pre-abort rejects before any backend call', async () => {
    const { backend, invoke } = fixture(method)
    const controller = new AbortController()
    controller.abort()
    await expect(invoke({ signal: controller.signal })).rejects.toMatchObject({ code: 'operation.aborted' })
    expect(backend[method]).not.toHaveBeenCalled()
  })
  test.each(['abort', 'deadline'])('%s bounds a held query and observes its late rejection', async kind => {
    let rejectNative
    const { backend, invoke } = fixture(
      method,
      () =>
        new Promise((_, reject) => {
          rejectNative = reject
        })
    )
    const controller = new AbortController()
    let outcome
    const result = invoke({ signal: controller.signal, timeoutMs: 10 }).then(
      value => {
        outcome = { value }
      },
      error => {
        outcome = { error }
      }
    )
    await jest.advanceTimersByTimeAsync(0)
    expect(backend[method]).toHaveBeenCalledTimes(1)
    if (kind === 'abort') controller.abort()
    else await jest.advanceTimersByTimeAsync(10)
    await jest.advanceTimersByTimeAsync(0)
    try {
      expect(outcome).toMatchObject({ error: { code: kind === 'abort' ? 'operation.aborted' : 'operation.timed-out' } })
    } finally {
      rejectNative(new Error('late native refusal'))
      await result
      await jest.advanceTimersByTimeAsync(0)
    }
  })
  test('timely result preserves the one normalized deadline', async () => {
    const { backend, invoke } = fixture(method)
    await expect(invoke({ timeoutMs: 10 })).resolves.toEqual(empty(method))
    const options = backend[method].mock.calls[0].at(-1)
    expect(options.deadline).toBe(1010)
  })
  test('native failure identity is not replaced by a budget error', async () => {
    const error = contractError('peer.not-found', 'connection', 'native.peers.lookup')
    const { invoke } = fixture(method, () => Promise.reject(error))
    await expect(invoke({ timeoutMs: 10 })).rejects.toMatchObject({
      code: 'peer.not-found',
      domain: 'connection',
      operation: 'native.peers.lookup'
    })
  })
  test('synchronous backend abort still observes the subsequently rejected query', async () => {
    const controller = new AbortController()
    const { invoke } = fixture(method, () => {
      controller.abort()
      return Promise.reject(new Error('native refusal after synchronous abort'))
    })
    await expect(invoke({ signal: controller.signal, timeoutMs: 10 })).rejects.toMatchObject({
      code: 'operation.aborted'
    })
    await jest.advanceTimersByTimeAsync(0)
  })
  test('synchronous native throw preserves the original error identity', async () => {
    const error = new Error('synchronous native failure')
    const { invoke } = fixture(method, () => {
      throw error
    })
    await expect(invoke({ timeoutMs: 10 })).rejects.toBe(error)
  })
})

test('an absent directory retains its explicit unsupported policy', async () => {
  const directory = createPublicPeerDirectory(undefined, () => Date.now())
  const controller = new AbortController()
  controller.abort()
  for (const method of methods)
    await expect(call(directory, method, { signal: controller.signal })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
})

describe('query filter validation before dispatch', () => {
  test.each([
    ['services', '180d', 'argument.invalid'],
    ['services', null, 'argument.invalid'],
    ['services', { 0: '180d', length: 1 }, 'argument.invalid'],
    ['services', Array(1), 'argument.invalid'],
    ['services', [undefined], 'argument.invalid'],
    ['services', [true], 'argument.invalid'],
    ['services', [-1], 'argument.invalid'],
    ['services', [0x100000000], 'argument.invalid'],
    ['services', ['not-a-uuid'], 'argument.invalid'],
    ['sources', 'system-connected', 'peer.reference-invalid'],
    ['sources', null, 'peer.reference-invalid'],
    ['sources', { length: 1 }, 'peer.reference-invalid'],
    ['sources', Array(1), 'peer.reference-invalid'],
    ['sources', ['toString'], 'peer.reference-invalid'],
    ['sources', ['unknown'], 'peer.reference-invalid'],
    ['references', reference, 'peer.reference-invalid'],
    ['references', null, 'peer.reference-invalid'],
    ['references', Array(1), 'peer.reference-invalid'],
    ['references', [undefined], 'peer.reference-invalid']
  ])('%s rejects malformed %p without calling a backend', async (key, value, code) => {
    const { backend, invoke } = fixture('connected')
    await expect(invoke({ [key]: value })).rejects.toMatchObject({ code })
    expect(backend.connected).not.toHaveBeenCalled()
  })

  test('numeric UUID widths use the canonical input converter', async () => {
    const { backend, invoke } = fixture('connected')
    await invoke({ services: [1, 0xffff, 0x10000], timeoutMs: 10 })
    expect(backend.connected.mock.calls[0][0]).toMatchObject({
      services: [
        '00000001-0000-1000-8000-00805f9b34fb',
        '0000ffff-0000-1000-8000-00805f9b34fb',
        '00010000-0000-1000-8000-00805f9b34fb'
      ],
      deadline: 1010
    })
  })

  test('backend query arrays and reference values are independent snapshots', async () => {
    const { backend, invoke } = fixture('connected')
    const originalReference = { ...reference }
    const options = { sources: ['system-connected'], services: ['180d'], references: [originalReference] }
    const result = invoke(options)
    options.sources[0] = 'backend-cache'
    options.services[0] = '180f'
    originalReference.opaqueId = 'changed'
    options.references.length = 0
    await result
    expect(backend.connected.mock.calls[0][0]).toMatchObject({
      sources: ['system-connected'],
      services: ['0000180d-0000-1000-8000-00805f9b34fb'],
      references: [reference]
    })
  })
})
