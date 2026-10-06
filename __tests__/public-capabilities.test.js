const { createPublicBleManager } = require('../src/public/ble-manager')
const { opaqueId } = require('../src/backend-contract/primitives')

function testManagerHostOptions() {
  return { peerId: value => opaqueId(value, 'peer', 'public-capabilities-test') }
}

describe('public BleCapabilities', () => {
  test.each([
    ['unavailable', 'capability.unavailable'],
    ['unsupported', 'capability.unsupported'],
    [null, 'capability.unsupported']
  ])('deferred connect preserves the %s capability verdict before dispatch', async (state, code) => {
    const connect = jest.fn()
    const direct = { id: 'connection:direct', state: 'supported' }
    const deferred = state === null ? null : { id: 'connection:when-available', state }
    const internal = {
      supports: () => false,
      capability: id => (id === direct.id ? direct : id === deferred?.id ? deferred : null),
      capabilities: () => (deferred === null ? [direct] : [direct, deferred]),
      destroy: jest.fn(),
      scan: jest.fn(),
      connect
    }
    const manager = await createPublicBleManager(internal, () => 0, testManagerHostOptions())
    await expect(manager.connect('peer-deferred', { intent: 'when-available' })).rejects.toMatchObject({
      code,
      operation: 'public-ble-manager.connect.when-available'
    })
    expect(connect).not.toHaveBeenCalled()
  })

  test.each(['supported', 'limited'])(
    'deferred connect admits the %s descriptor without a boolean second opinion',
    async state => {
      const connect = jest.fn(async () => ({
        disconnect: async () => ({ state: 'released', failures: [] }),
        release: async () => ({ state: 'released', failures: [] })
      }))
      const direct = { id: 'connection:direct', state: 'supported' }
      const deferred = { id: 'connection:when-available', state }
      const internal = {
        supports: () => false,
        capability: id => (id === direct.id ? direct : id === deferred.id ? deferred : null),
        capabilities: () => [direct, deferred],
        destroy: jest.fn(),
        scan: jest.fn(),
        connect
      }
      const manager = await createPublicBleManager(internal, () => 0, testManagerHostOptions())
      const connection = await manager.connect('peer-deferred', { intent: 'when-available' })
      expect(connect).toHaveBeenCalledTimes(1)
      await connection.release()
    }
  )

  test('rejects direct connections when the backend omits the direct capability descriptor', async () => {
    const connect = jest.fn()
    const internal = {
      supports: () => false,
      capability: () => null,
      capabilities: () => [],
      destroy: jest.fn(),
      scan: jest.fn(),
      connect
    }
    const manager = await createPublicBleManager(internal, () => 0)

    await expect(manager.connect('peer-without-capability')).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(connect).not.toHaveBeenCalled()
  })

  test('rejects direct connections when the backend explicitly marks them unsupported', async () => {
    const connect = jest.fn()
    const internal = {
      supports: () => false,
      capability: id => (id === 'connection:direct' ? { id, state: 'unsupported' } : null),
      capabilities: () => [{ id: 'connection:direct', state: 'unsupported' }],
      destroy: jest.fn(),
      scan: jest.fn(),
      connect
    }
    const manager = await createPublicBleManager(internal, () => 0)

    await expect(manager.connect('peer-direct')).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(connect).not.toHaveBeenCalled()
  })

  test('projects negotiated capabilities without exposing backend manager methods', async () => {
    const supported = { id: 'gatt:descriptors', state: 'supported' }
    const limited = {
      id: 'gatt:indications',
      state: 'limited',
      limitations: [{ code: 'no-indication-bit', explanation: 'test limitation', affectedGuarantee: 'delivery' }]
    }
    const direct = {
      id: 'connection:direct',
      state: 'limited',
      limitations: [{ code: 'deterministic-only', explanation: 'test limitation', affectedGuarantee: 'radio' }]
    }
    const descriptors = [supported, limited, direct]
    const internal = {
      supports: id => id === supported.id || id === limited.id,
      capability: id => descriptors.find(descriptor => descriptor.id === id) ?? null,
      capabilities: () => descriptors,
      destroy: jest.fn(),
      scan: jest.fn(),
      connect: jest.fn()
    }

    const manager = await createPublicBleManager(internal, () => 0, testManagerHostOptions())

    expect(manager.capabilities.supports('gatt:descriptors')).toBe(true)
    expect(manager.capabilities.supports('gatt:indications')).toBe(true)
    expect(manager.capabilities.get('gatt:descriptors')).toBe(supported)
    expect(manager.capabilities.get('feature:missing')).toBeUndefined()
    expect(manager.capabilities.list()).toEqual(descriptors)
    expect(manager.capabilities.require('gatt:indications')).toBe(limited)
    expect(() => manager.capabilities.require('feature:missing')).toThrow('capability.unsupported')

    internal.connect.mockResolvedValue({
      disconnect: async () => ({ state: 'released', failures: [] }),
      release: async () => ({ state: 'released', failures: [] })
    })
    const peer = { id: 'peer-1', name: 'Original', rssi: -40 }
    const connection = await manager.connect(peer)
    peer.name = 'Mutated'
    expect(connection.peer).toEqual({
      id: 'peer-1',
      name: 'Original',
      rssi: -40,
      reference: null,
      sources: [],
      lastAdvertisement: null
    })
    expect(Object.isFrozen(connection.peer)).toBe(true)
    await connection.release()

    internal.connect.mockResolvedValue({
      disconnect: async () => ({ state: 'released', failures: [] }),
      release: async () => {
        throw new Error('release-failed')
      }
    })
    await expect(
      manager.withConnection('peer-2', {}, async () => {
        throw new Error('action-failed')
      })
    ).rejects.toMatchObject({
      constructor: AggregateError,
      errors: [new Error('action-failed'), new Error('release-failed')]
    })

    internal.connect.mockResolvedValue({
      disconnect: async () => ({ state: 'released', failures: [] }),
      release: async () => ({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'connection',
            error: {
              code: 'connection.lost',
              domain: 'connection',
              operation: 'public-capabilities.cleanup',
              platform: null,
              retryability: 'never'
            }
          }
        ]
      })
    })
    await expect(manager.withConnection('peer-3', {}, async () => 'ok')).rejects.toThrow('BLE cleanup failed')
    await expect(manager.connect('peer-4', { timeoutMs: 0 })).rejects.toMatchObject({ code: 'argument.invalid' })
  })
})
