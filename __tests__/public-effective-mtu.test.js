const { createPublicBleManager } = require('../src/public/ble-manager')
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
