// __tests__/backends/reactnative/ordered-setup.test.js
const { setupReactNativeOrdered } = require('../../../src/backends/reactnative/react-native-ordered-setup')
const { contractError } = require('../../../src/backend-contract/errors')
const opts = { signal: null, deadline: null, delivery: { itemCapacity: 4, byteCapacity: 64, reservedControlCapacity: 4 } }
const member = { memberId: 'a', path: {}, deliveryMode: 'prefer-notification', selector: {} }
function fixture(invoke, open) {
  return { invoke, open, operationId: () => 'op', validate: () => {}, peerId: 'peer', connectionGeneration: '1', databaseGeneration: '1', publicConnectionGeneration: 'c', publicDatabaseGeneration: 'd', members: [member], options: opts, retain: jest.fn() }
}

test('incompatible ordered wire refuses before reservation', async () => {
  const invoke = jest.fn(async () => ({ wireVersion: 1 }))
  await expect(setupReactNativeOrdered(fixture(invoke, jest.fn()))).rejects.toThrow()
  expect(invoke.mock.calls.map(call => call[0])).toEqual(['gatt.ordered.version'])
})

test('failed setup retains observations and retryable cleanup', async () => {
  let polled = false
  const invoke = jest.fn(async op => {
    if (op === 'gatt.ordered.version') return { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') return { versions: [1] }
    if (op === 'gatt.ordered.reserve') return { collectionId: '1', reclaimCapability: 'cap' }
    if (op === 'gatt.ordered.status') return { sourceGeneration: null, members: [{ memberId: '0', acquisition: 'indeterminate', effectiveDelivery: 'unknown', cleanupReleased: false, cleanupError: null }], cleanupComplete: false, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.close') return 'pending-source'
    if (op === 'gatt.ordered.poll') { if (polled) return { deliveryId: null, record: '{"kind":"empty"}' }; polled = true; return { deliveryId: '1', record: JSON.stringify({ kind: 'loss', loss: { scope: { kind: 'unknown' }, unit: 'source-records', count: null, bytes: null } }) } }
    return null
  })
  const input = fixture(invoke, async () => { throw contractError('operation.aborted', 'gatt', 'test') })
  const setup = await setupReactNativeOrdered(input)
  expect(setup.state).toBe('failed')
  expect(setup.error.code).toBe('operation.aborted')
  expect(setup.observations).toMatchObject([{ kind: 'loss', lostSourceRecords: null }])
  expect(setup.cleanup.group.state).toBe('release-failed')
  expect(input.retain).toHaveBeenCalledTimes(1)
})

test('abort after reserve before open retires the empty native inventory and receipt', async () => {
  let closed = false
  const invoke = jest.fn(async op => {
    if (op === 'gatt.ordered.version') return { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') return { versions: [1] }
    if (op === 'gatt.ordered.reserve') return { collectionId: '1', reclaimCapability: 'cap' }
    if (op === 'gatt.ordered.status') return { sourceGeneration: null, members: [], cleanupComplete: closed, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.close') { closed = true; return 'released' }
    if (op === 'gatt.ordered.poll') throw new Error('unopened group has no records')
    if (op === 'gatt.ordered.ack-group' || op === 'gatt.ordered.forget-group') return null
    throw new Error(`unexpected ${op}`)
  })
  const input = fixture(invoke, async () => { throw contractError('operation.aborted', 'gatt', 'test') })
  const setup = await setupReactNativeOrdered(input)
  expect(setup).toMatchObject({ state: 'failed', error: { code: 'operation.aborted' }, members: [{ memberId: 'a', acquisition: 'not-started' }], observations: [], cleanup: { group: { state: 'released' } } })
  expect(invoke.mock.calls.map(call => call[0])).toEqual(['gatt.ordered.version', 'gatt.ordered.failure-metadata.version', 'gatt.ordered.reserve', 'gatt.ordered.status', 'gatt.ordered.close', 'gatt.ordered.status', 'gatt.ordered.ack-group', 'gatt.ordered.forget-group'])
  expect(invoke.mock.calls.at(-1)[1].reclaimCapability).toBe('cap')
})

test('partial setup retains acknowledged prefix when a later delivery fails decoding', async () => {
  let cursor = 0
  let malformed = true
  const nativeValue = (ordinal, byte) => JSON.stringify({ kind: 'value', memberId: '0', ordinal, sourceMembership: { member: '0', epoch: '1' }, bytes: [byte], delivery: 'notification', provenance: 'notification' })
  const invoke = jest.fn(async (op, args) => {
    if (op === 'gatt.ordered.version') return { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') return { versions: [1] }
    if (op === 'gatt.ordered.reserve') return { collectionId: '1', reclaimCapability: 'cap' }
    if (op === 'gatt.ordered.status') return { sourceGeneration: { kind: 'counter', value: '1' }, members: [{ memberId: '0', acquisition: 'indeterminate', effectiveDelivery: 'notification', cleanupReleased: false, cleanupError: null }], cleanupComplete: false, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.close') return 'pending-source'
    if (op === 'gatt.ordered.poll') return cursor === 0 ? { deliveryId: '1', record: nativeValue('1', 5) } : { deliveryId: '2', record: malformed ? '{' : nativeValue('2', 6) }
    if (op === 'gatt.ordered.ack-delivery') { expect(args.deliveryId).toBe(String(cursor + 1)); cursor++; return null }
    throw new Error(`unexpected ${op}`)
  })
  const setup = await setupReactNativeOrdered(fixture(invoke, async () => { throw contractError('operation.aborted', 'gatt', 'test') }))
  expect(setup.state).toBe('failed')
  expect(setup.observations).toMatchObject([{ kind: 'value', ordinal: '1', memberId: 'a' }])
  expect(setup.cleanup.group.failures.some(failure => failure.resourceKind === 'ordered-observations')).toBe(true)
  expect(cursor).toBe(1)
  malformed = false
  const next = await setup.stream[Symbol.asyncIterator]().next()
  expect(next.value).toMatchObject({ kind: 'value', ordinal: '2', memberId: 'a' })
  expect(cursor).toBe(2)
})

test('unopened cleanup refusal remains retryable on the same exact reservation', async () => {
  let attempts = 0
  let closed = false
  const invoke = jest.fn(async op => {
    if (op === 'gatt.ordered.version') return { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') return { versions: [1] }
    if (op === 'gatt.ordered.reserve') return { collectionId: '8', reclaimCapability: 'secret' }
    if (op === 'gatt.ordered.status') return { sourceGeneration: null, members: [], cleanupComplete: closed, setupError: null, fenceError: null }
    if (op === 'gatt.ordered.close') { attempts++; if (attempts === 1) throw contractError('platform.failure', 'cleanup', 'test.close'); closed = true; return 'released' }
    if (op === 'gatt.ordered.poll') return { deliveryId: null, record: '{"kind":"empty"}' }
    if (op === 'gatt.ordered.ack-group' || op === 'gatt.ordered.forget-group') return null
    throw new Error(`unexpected ${op}`)
  })
  const setup = await setupReactNativeOrdered(fixture(invoke, async () => { throw contractError('operation.aborted', 'gatt', 'test') }))
  expect(setup.cleanup.group.state).toBe('release-failed')
  expect(invoke.mock.calls.filter(call => call[0] === 'gatt.ordered.ack-group')).toHaveLength(0)
  expect((await setup.close()).group.state).toBe('released')
  const forget = invoke.mock.calls.find(call => call[0] === 'gatt.ordered.forget-group')
  expect(forget[1]).toEqual({ peerId: 'peer', collectionId: '8', reclaimCapability: 'secret' })
})


test.each([{}, { versions: [] }, { versions: [3] }])('missing or unsupported metadata revision refuses before reservation: %j', async offer => {
  const invoke = jest.fn(async op => op === 'gatt.ordered.version' ? { wireVersion: 2 } : offer)
  const input = fixture(invoke, jest.fn())
  await expect(setupReactNativeOrdered(input)).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
  expect(invoke.mock.calls.map(call => call[0])).toEqual(['gatt.ordered.version', 'gatt.ordered.failure-metadata.version'])
  expect(input.open).not.toHaveBeenCalled()
})

test('negotiates v1 explicitly on admission when offered', async () => {
  const invoke = jest.fn(async op => {
    if (op === 'gatt.ordered.version') return { wireVersion: 2 }
    if (op === 'gatt.ordered.failure-metadata.version') return { versions: [1] }
    if (op === 'gatt.ordered.reserve') return { collectionId: '1', reclaimCapability: 'cap' }
    if (op === 'gatt.ordered.status') return { sourceGeneration: { kind: 'counter', value: '1' }, members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'notification', cleanupReleased: false, cleanupError: null }], cleanupComplete: false, setupError: null, fenceError: null }
    throw new Error(op)
  })
  const open = jest.fn(async () => {})
  expect((await setupReactNativeOrdered(fixture(invoke, open))).state).toBe('ready')
  expect(open).toHaveBeenCalledWith(expect.objectContaining({ failureMetadataVersion: 1 }))
})
