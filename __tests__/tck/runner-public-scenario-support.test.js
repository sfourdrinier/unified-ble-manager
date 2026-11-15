const { connectToDeterministicPeer, scanOptions } = require('../../src/tck/runner-public-scenario-support')

test('peer acquisition reserves room for one desktop fixture and its terminal control record', async () => {
  const peerId = 'fixture-peer'
  const connection = { peerId }
  const scan = {
    observations: { async *[Symbol.asyncIterator]() { yield { kind: 'value', value: { device: { id: peerId } } } } },
    stop: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
  const manager = {
    scan: jest.fn(async () => scan),
    connect: jest.fn(async () => connection)
  }
  const fixture = { controller: { settle: promise => promise, perform: async () => {}, flush: async () => {} } }

  await expect(connectToDeterministicPeer(manager, fixture, { id: 'setup' })).resolves.toBe(connection)
  expect(manager.scan.mock.calls[0][0].delivery.byteCapacity).toBe(513)
  expect(scanOptions(false).delivery.byteCapacity).toBe(513)
  expect(manager.connect).toHaveBeenCalledWith(peerId, expect.objectContaining({ intent: 'direct' }))
  expect(scan.stop).toHaveBeenCalledTimes(1)
})
