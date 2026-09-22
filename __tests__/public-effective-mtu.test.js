const { createPublicBleManager } = require('../src/public/ble-manager')
const { contractError } = require('../src/backend-contract/errors')
const { opaqueId } = require('../src/backend-contract/primitives')

function testManagerHostOptions() {
  return { peerId: value => opaqueId(value, 'peer', 'public-effective-mtu-test') }
}

function capability(state) {
  return { state, limitations: [{ code: 'test', explanation: 'test', affectedGuarantee: 'test' }] }
}

function terminal() {
  return { correlation: 'operation-1', outcome: 'succeeded', cause: null }
}

function createInternal() {
  let value = null
  const descriptors = new Map([
    ['connection:direct', capability('supported')],
    ['connection:effective-mtu', capability('limited')]
  ])
  const connection = {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    events: {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined })
      })
    },
    effectiveMtu: async () => ({
      connectionId: 'connection-1',
      connectionGeneration: 'generation-1',
      attMtu: value,
      payloadBytes: value === null ? null : value - 3,
      platformPduBytes: null,
      observedAtMonotonicMs: value === null ? 10 : 20,
      terminal: terminal()
    })
  }
  return {
    manager: {
      capability: id => descriptors.get(id) ?? null,
      supports: id => descriptors.get(id)?.state === 'supported' || descriptors.get(id)?.state === 'limited',
      connect: async () => connection
    },
    measure(mtu) {
      value = mtu
    }
  }
}

describe('public effective MTU control', () => {
  test('returns unavailable before measurement and measured values after Android observation', async () => {
    const internal = createInternal()
    const manager = await createPublicBleManager(internal.manager, () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')

    await expect(connection.controls.effectiveMtu()).resolves.toMatchObject({
      state: 'unavailable',
      attMtu: null,
      payloadBytes: null,
      platformPduBytes: null,
      connectionGeneration: 'generation-1'
    })

    internal.measure(185)
    await expect(connection.controls.effectiveMtu()).resolves.toMatchObject({
      state: 'measured',
      attMtu: 185,
      payloadBytes: 182,
      platformPduBytes: null,
      connectionGeneration: 'generation-1'
    })
  })
})

/**
 * Production connections are class instances whose methods read their own
 * receiver (the React Native Rust connection dispatches through `this`). The
 * public controls must invoke them as methods, never detached.
 */
class ReceiverBoundConnection {
  constructor() {
    this.connectionId = 'connection-1'
    this.connectionGeneration = 'generation-1'
    this.attMtu = 247
    this.events = {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined })
      })
    }
  }

  async effectiveMtu() {
    return {
      connectionId: this.connectionId,
      connectionGeneration: this.connectionGeneration,
      attMtu: this.attMtu,
      payloadBytes: this.attMtu - 3,
      platformPduBytes: null,
      observedAtMonotonicMs: 30,
      terminal: terminal()
    }
  }

  async writeWithoutResponseReadiness() {
    const observation = {
      connectionId: this.connectionId,
      connectionGeneration: this.connectionGeneration,
      ready: true,
      observedAtMonotonicMs: 31,
      ordinal: 1
    }
    let delivered = false
    return {
      events: {
        [Symbol.asyncIterator]: () => ({
          next: async () => {
            if (delivered) return { done: true, value: undefined }
            delivered = true
            return { done: false, value: { kind: 'value', value: observation } }
          },
          return: async () => ({ done: true, value: undefined })
        })
      },
      close: async () => ({ state: 'released', failures: [] })
    }
  }
}

describe('public controls invoke receiver-bound connection methods', () => {
  function receiverBoundManager() {
    const descriptors = new Map([
      ['connection:direct', capability('supported')],
      ['connection:effective-mtu', capability('limited')],
      ['gatt:write-without-response-readiness', capability('limited')]
    ])
    const connection = new ReceiverBoundConnection()
    return {
      capability: id => descriptors.get(id) ?? null,
      supports: id => descriptors.get(id)?.state === 'supported' || descriptors.get(id)?.state === 'limited',
      connect: async () => connection
    }
  }

  test('effectiveMtu answers from the connection method instead of a detached TypeError', async () => {
    const manager = await createPublicBleManager(receiverBoundManager(), () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')
    await expect(connection.controls.effectiveMtu()).resolves.toMatchObject({
      state: 'measured',
      attMtu: 247,
      payloadBytes: 244
    })
  })

  test('writeReadiness opens the watch through the connection method', async () => {
    const manager = await createPublicBleManager(receiverBoundManager(), () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')
    const iterator = connection.controls.writeReadiness('without-response')[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({ done: false, value: { state: 'measured', ready: true } })
    await iterator.return()
  })
})

/**
 * FX9: `effectiveMtu` honours caller options like every sibling radio read.
 * The fake internal connection applies core semantics: it records the
 * normalized options, rejects a pre-aborted call, and rejects an in-flight
 * call when the caller's signal aborts.
 */
function createCancellableInternal() {
  const descriptors = new Map([
    ['connection:direct', capability('supported')],
    ['connection:effective-mtu', capability('limited')]
  ])
  const seen = []
  const connection = {
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    events: {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined })
      })
    },
    effectiveMtu: options => {
      seen.push(options)
      if (options?.signal?.aborted === true) {
        return Promise.reject(contractError('operation.aborted', 'connection', 'fx9-test.effective-mtu'))
      }
      return new Promise((resolve, reject) => {
        const signal = options?.signal ?? null
        if (signal !== null) {
          signal.addEventListener(
            'abort',
            () => reject(contractError('operation.aborted', 'connection', 'fx9-test.effective-mtu')),
            { once: true }
          )
        }
        // Never resolves on its own: the test either aborts or inspects `seen`.
      })
    }
  }
  return {
    manager: {
      capability: id => descriptors.get(id) ?? null,
      supports: id => descriptors.get(id)?.state === 'supported' || descriptors.get(id)?.state === 'limited',
      connect: async () => connection
    },
    seen
  }
}

describe('FX9: effectiveMtu honours caller options', () => {
  test("the caller's signal reaches the internal connection", async () => {
    const internal = createCancellableInternal()
    const manager = await createPublicBleManager(internal.manager, () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')
    const controller = new AbortController()
    const pending = connection.controls.effectiveMtu({ signal: controller.signal })
    pending.catch(() => undefined)
    let settled = false
    pending.then(
      () => {
        settled = true
      },
      () => {
        settled = true
      }
    )
    await new Promise(resolve => setImmediate(resolve))
    expect(settled).toBe(false)
    expect(internal.seen).toHaveLength(1)
    expect(internal.seen[0]?.signal).toBe(controller.signal)
    controller.abort()
    await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
  })

  test('timeoutMs becomes a monotonic deadline', async () => {
    const internal = createCancellableInternal()
    const manager = await createPublicBleManager(internal.manager, () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')
    const pending = connection.controls.effectiveMtu({ timeoutMs: 5000 })
    pending.catch(() => undefined)
    expect(internal.seen).toHaveLength(1)
    expect(Number(internal.seen[0]?.deadline)).toBe(5100)
  })

  test('a pre-aborted signal fails before measuring', async () => {
    const internal = createCancellableInternal()
    const manager = await createPublicBleManager(internal.manager, () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')
    const controller = new AbortController()
    controller.abort()
    await expect(connection.controls.effectiveMtu({ signal: controller.signal })).rejects.toMatchObject({
      code: 'operation.aborted'
    })
  })

  test('a non-signal is rejected as an invalid argument like readRssi', async () => {
    const internal = createCancellableInternal()
    const manager = await createPublicBleManager(internal.manager, () => 100, testManagerHostOptions())
    const connection = await manager.connect('peer-1')
    await expect(connection.controls.effectiveMtu({ signal: 'not-a-signal' })).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(internal.seen).toHaveLength(0)
  })
})
