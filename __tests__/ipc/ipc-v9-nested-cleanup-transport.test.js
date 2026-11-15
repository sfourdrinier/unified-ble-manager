const { IpcGattDatabase } = require('../../src/ipc/manager')

const entries = [
  { key: '__proto__', value: { kind: 'int', value: '-9223372036854775808' } },
  { key: '__proto__', value: { kind: 'text', value: 'evidence' } }
]
const error = operation => ({
  code: 'platform.failure',
  domain: 'gatt',
  operation,
  platform: { domain: 'native', code: 'failure', safeMessage: '', metadata: {}, metadataEntries: entries },
  retryability: 'never'
})
const member = {
  memberId: '0',
  acquisition: 'acquired',
  effectiveDelivery: 'notification',
  cleanupReleased: false,
  cleanupError: error('member.cleanup')
}
const status = fenceError => JSON.stringify({ members: [member], sourceGeneration: null, setupError: null, fenceError })

test('native ordered close preserves fence and member cleanup causes independently', async () => {
  const manager = {
    nextOrderedHandle: 1,
    bootstrap: {
      rendererLease: { leaseId: 'lease', generation: '1' },
      attachment: {
        attachmentId: 'a',
        backendInstanceId: 'b',
        backendGeneration: 'bg',
        adapter: { adapterId: 'a', adapterGeneration: 'ag' }
      }
    },
    route: async (command, payload) => {
      if (command === 'gatt.ordered.open')
        return {
          handle: payload.orderedHandle,
          state: 'native',
          nativeStatus: status(null),
          collectionId: '7',
          connectionGeneration: 'cg',
          databaseGeneration: 'dg',
          observations: [],
          uncertainTail: false,
          cleanupState: 'pending-source'
        }
      if (command === 'gatt.ordered.close')
        return { nativeStatus: status(error('fence.cleanup')), cleanupState: 'pending-source' }
      throw new Error(command)
    }
  }
  const connection = {
    handle: 'c',
    peerId: 'p',
    connectionId: 'cid',
    ownerLeaseId: 'lease',
    connectionGeneration: 'cg',
    registerDatabase() {}
  }
  const database = IpcGattDatabase.fromPayload(manager, connection, {
    schemaVersion: 2,
    handle: 'd',
    databaseId: 'did',
    databaseGeneration: 'dg',
    services: [{ uuid: '180d', occurrence: '0', primary: true, includedServices: [] }],
    characteristics: [
      {
        handle: 'ch',
        serviceUuid: '180d',
        serviceOccurrence: '0',
        characteristicUuid: '2a37',
        characteristicOccurrence: '0',
        properties: ['notify']
      }
    ],
    descriptors: []
  })
  const path = (await database.snapshot()).characteristics[0].path
  const setup = await database.subscribeOrdered([{ memberId: 'A', path, deliveryMode: 'prefer-notification' }], {
    deadline: null,
    delivery: { itemCapacity: 8, byteCapacity: 1024, reservedControlCapacity: 1, overflowPolicy: 'error' }
  })
  const cleanup = await setup.stream.close()
  expect(cleanup.group.failures).toEqual(
    expect.arrayContaining([
      expect.objectContaining({
        error: expect.objectContaining({
          operation: 'fence.cleanup',
          platform: expect.objectContaining({ metadataEntries: entries })
        })
      })
    ])
  )
  expect(cleanup.members[0].cleanup.failures[0].error).toMatchObject(error('member.cleanup'))
})
