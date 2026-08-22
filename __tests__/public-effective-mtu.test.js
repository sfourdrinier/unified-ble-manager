const { createPublicBleManager } = require('../src/public/ble-manager')

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
    const manager = await createPublicBleManager(internal.manager, () => 100)
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
