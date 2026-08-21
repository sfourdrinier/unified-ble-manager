const { createPublicBleManager } = require('../src/public/ble-manager')

describe('public BleCapabilities', () => {
  test('projects negotiated capabilities without exposing backend manager methods', async () => {
    const supported = { id: 'gatt:descriptors', state: 'supported' }
    const limited = {
      id: 'gatt:indications',
      state: 'limited',
      limitations: [{ code: 'no-indication-bit', explanation: 'test limitation', affectedGuarantee: 'delivery' }]
    }
    const descriptors = [supported, limited]
    const internal = {
      supports: id => id === supported.id || id === limited.id,
      capability: id => descriptors.find(descriptor => descriptor.id === id) ?? null,
      capabilities: () => descriptors,
      destroy: jest.fn(),
      scan: jest.fn(),
      connect: jest.fn()
    }

    const manager = await createPublicBleManager(internal, () => 0)

    expect(manager.capabilities.supports('gatt:descriptors')).toBe(true)
    expect(manager.capabilities.supports('gatt:indications')).toBe(false)
    expect(manager.capabilities.get('gatt:descriptors')).toBe(supported)
    expect(manager.capabilities.get('feature:missing')).toBeUndefined()
    expect(manager.capabilities.list()).toEqual(descriptors)
    try {
      manager.capabilities.require('gatt:indications')
      throw new Error('expected limited capability rejection')
    } catch (error) {
      expect(error).toMatchObject({
        code: 'capability.limited',
        limitations: limited.limitations
      })
    }
    expect(() => manager.capabilities.require('feature:missing')).toThrow('capability.unsupported')

    internal.connect.mockResolvedValue({
      disconnect: async () => ({ state: 'released', failures: [] }),
      release: async () => ({ state: 'released', failures: [] })
    })
    const peer = { id: 'peer-1', name: 'Original', rssi: -40 }
    const connection = await manager.connect(peer)
    peer.name = 'Mutated'
    expect(connection.peer).toEqual({ id: 'peer-1', name: 'Original', rssi: -40 })
    expect(Object.isFrozen(connection.peer)).toBe(true)
    await connection.release()
  })
})
