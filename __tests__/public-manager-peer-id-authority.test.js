// __tests__/public-manager-peer-id-authority.test.js

const { createPublicBleManager } = require('../src/public/ble-manager')
const { opaqueId } = require('../src/backend-contract/primitives')

test('projects a lightweight manager with an explicit peer-id authority', async () => {
  const connect = jest.fn(async () => ({
    connectionId: 'connection-1',
    connectionGeneration: 'generation-1',
    events: { [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }) }) },
    release: async () => ({ state: 'released', failures: [] }),
    disconnect: async () => ({ state: 'released', failures: [] })
  }))
  const internal = {
    supports: id => id === 'connection:direct',
    capability: id => (id === 'connection:direct' ? { id, state: 'supported' } : null),
    capabilities: () => [],
    connect,
    destroy: async () => ({ state: 'released', failures: [] }),
    scan: jest.fn()
  }
  const manager = await createPublicBleManager(internal, () => 0, {
    peerId: () => opaqueId('sentinel-peer-id', 'peer', 'public-manager-test')
  })

  await expect(manager.connect('peer-1')).resolves.toMatchObject({ peer: { id: 'peer-1' } })
  expect(connect).toHaveBeenCalledTimes(1)
  expect(connect.mock.calls[0][0]).toBe('sentinel-peer-id')
})

test('fails closed when a supported direct manager has no peer-id authority', async () => {
  const connect = jest.fn()
  const internal = {
    supports: id => id === 'connection:direct',
    capability: id => (id === 'connection:direct' ? { id, state: 'supported' } : null),
    capabilities: () => [],
    connect,
    destroy: async () => ({ state: 'released', failures: [] }),
    scan: jest.fn()
  }
  const manager = await createPublicBleManager(internal, () => 0)

  await expect(manager.connect('peer-1')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(connect).not.toHaveBeenCalled()
})
