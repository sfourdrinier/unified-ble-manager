// __tests__/backends/reactnative/ordered-transport.test.js
const { createReactNativeOrderedTransport } = require('../../../src/backends/reactnative/react-native-ordered-transport')

test('ordered transport scopes operations and authenticates final receipt retirement', async () => {
  const invoke = jest.fn(async op => op === 'gatt.ordered.status' ? {
    sourceGeneration: { kind: 'counter', value: '18446744073709551615' },
    members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'notification', cleanupReleased: true, cleanupError: null }],
    cleanupComplete: true, setupError: null, fenceError: null
  } : null)
  const transport = createReactNativeOrderedTransport(invoke, 'peer', '3', 'secret', ['member'], () => 'op')
  expect((await transport.status()).members[0].memberId).toBe('member')
  await transport.acknowledgeOrderedDelivery({ peerId: 'peer', collectionId: '3', deliveryId: '18446744073709551615' })
  await transport.closeOrderedGroup({ peerId: 'peer', collectionId: '3' })
  await transport.acknowledgeOrderedGroup({ peerId: 'peer', collectionId: '3' })
  await transport.forgetOrderedGroup({ peerId: 'peer', collectionId: '3' })
  expect(invoke.mock.calls[1]).toEqual(['gatt.ordered.ack-delivery', { peerId: 'peer', collectionId: '3', deliveryId: '18446744073709551615' }])
  expect(invoke.mock.calls[2]).toEqual(['gatt.ordered.close', { peerId: 'peer', collectionId: '3', operationId: 'op' }])
  expect(invoke.mock.calls[4]).toEqual(['gatt.ordered.forget-group', { peerId: 'peer', collectionId: '3', reclaimCapability: 'secret' }])
})

test('ordered status rejects mismatched member identity', async () => {
  const invoke = async () => ({ sourceGeneration: null, members: [{ memberId: '1', acquisition: 'acquired', effectiveDelivery: 'unknown', cleanupReleased: true, cleanupError: null }], cleanupComplete: true, setupError: null, fenceError: null })
  const transport = createReactNativeOrderedTransport(invoke, 'p', '3', 'secret', ['member'], () => 'op')
  await expect(transport.status()).rejects.toThrow()
})

test('capacity recovery is version gated and fenced by the source generation', async () => {
  const invoke = jest.fn(async op => {
    if (op === 'gatt.ordered.capacity.version') return { wireVersion: 1, maximumMaxBytes: 4194304 }
    if (op === 'gatt.ordered.increase-capacity') return { previousMaxBytes: 1800, maxBytes: 8192, maximumMaxBytes: 4194304 }
    throw Error(`unexpected ${op}`)
  })
  const transport = createReactNativeOrderedTransport(invoke, 'p', '3', 'secret', ['member'], () => 'op')
  const generation = { kind: 'counter', value: '7' }
  await expect(transport.increaseByteCapacity(generation, 8192)).resolves.toEqual({ previousMaxBytes: 1800, maxBytes: 8192, maximumMaxBytes: 4194304 })
  expect(invoke.mock.calls).toEqual([
    ['gatt.ordered.capacity.version', {}],
    ['gatt.ordered.increase-capacity', { peerId: 'p', collectionId: '3', sourceGeneration: generation, maxBytes: 8192, wireVersion: 1 }]
  ])
})

test('ordered stream exposes the new byte budget after recovery', async () => {
  const { OrderedNativeStream } = require('../../../src/core/ordered-native-stream')
  const { capacity } = require('../../../src/backend-contract/primitives')
  const generation = { kind: 'counter', value: '7' }
  const increaseByteCapacity = jest.fn(async () => ({ previousMaxBytes: 32, maxBytes: 128, maximumMaxBytes: 4194304 }))
  const stream = new OrderedNativeStream({ itemCapacity: capacity(2), byteCapacity: capacity(32), reservedControlCapacity: capacity(1) }, { increaseByteCapacity }, 'p', '3', 'c', 'd', ['member'])
  stream.setSourceGeneration(generation)
  await stream.increaseByteCapacity(128)
  expect(increaseByteCapacity).toHaveBeenCalledWith(generation, 128)
  expect(stream.limits.byteCapacity).toBe(128)
})

test('unsupported recovery revision refuses before changing capacity', async () => {
  const invoke = jest.fn(async () => ({ wireVersion: 2, maximumMaxBytes: 4194304 }))
  const transport = createReactNativeOrderedTransport(invoke, 'p', '3', 'secret', ['member'], () => 'op')
  await expect(transport.increaseByteCapacity({ kind: 'counter', value: '7' }, 128)).rejects.toThrow('capability.unsupported')
  expect(invoke).toHaveBeenCalledTimes(1)
})

test('an older session reports capacity recovery as unsupported', async () => {
  const { contractError } = require('../../../src/backend-contract/errors')
  const invoke = jest.fn(async () => { throw contractError('argument.invalid', 'core', 'ubm-mobile.wire.op') })
  const transport = createReactNativeOrderedTransport(invoke, 'p', '3', 'secret', ['member'], () => 'op')
  await expect(transport.increaseByteCapacity({ kind: 'counter', value: '7' }, 128)).rejects.toThrow('capability.unsupported')
  expect(invoke).toHaveBeenCalledTimes(1)
})
