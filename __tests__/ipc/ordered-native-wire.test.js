// __tests__/ipc/ordered-native-wire.test.js

const { IpcGattDatabase } = require('../../src/ipc/manager')
const { OrderedDelivery } = require('../../src/ipc/ordered-delivery')

const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const status = JSON.stringify({
  members: [
    {
      memberId: '0',
      acquisition: 'acquired',
      effectiveDelivery: 'notification',
      cleanupReleased: true,
      cleanupError: null
    }
  ],
  sourceGeneration: { kind: 'counter', value: '18446744073709551615' },
  setupError: null,
  fenceError: null
})

const initialStatus = JSON.stringify({ ...JSON.parse(status), sourceGeneration: null })

test('shared IPC client decodes retained native wire with explicit acknowledgements', async () => {
  const source = (async function* () {
    yield {
      kind: 'value',
      memberId: '0',
      sourceMembership: { member: '0', epoch: '18446744073709551615' },
      ordinal: '18446744073709551615',
      bytes: [0, 255],
      delivery: 'notification',
      provenance: 'notification'
    }
    yield {
      kind: 'terminal',
      reason: { kind: 'closed' },
      uncertainTail: false,
      rejectedValues: null,
      rejectedLoss: null,
      sourceRejection: null,
      sourceFailureMember: null,
      secondaryFailure: null,
      additionalFailureCount: '0',
      failureSummaryVersion: 1,
      failureCountOverflowed: false,
      failureObservationsOmitted: false
    }
  })()
  const delivery = new OrderedDelivery(source)
  const calls = []
  const manager = {
    nextOrderedHandle: 1,
    bootstrap: {
      rendererLease: { leaseId: 'lease', generation: '1' },
      attachment: {
        attachmentId: 'attachment',
        backendInstanceId: 'backend',
        backendGeneration: 'bg',
        adapter: { adapterId: 'a', adapterGeneration: 'ag' }
      }
    },
    route: async (command, payload) => {
      calls.push({ command, payload })
      if (command === 'gatt.ordered.open')
        return {
          handle: payload.orderedHandle,
          state: 'native',
          nativeStatus: initialStatus,
          collectionId: '7',
          connectionGeneration: 'cg',
          databaseGeneration: 'dg',
          observations: [],
          uncertainTail: false,
          cleanupState: 'released'
        }
      if (command === 'gatt.ordered.next') {
        const poll = delivery.poll(payload.ackDeliveryId)
        return poll.state === 'item'
          ? { state: 'item', deliveryId: poll.deliveryId, nativeWire: poll.item, nativeStatus: status }
          : poll
      }
      if (command === 'gatt.ordered.close') return { nativeStatus: status, cleanupState: 'released' }
      if (command === 'gatt.ordered.forget') return { state: 'forgotten' }
      if (command === 'gatt.ordered.retire') return { state: 'retired' }
      throw new Error(command)
    }
  }
  const connection = {
    handle: 'connection',
    peerId: 'peer',
    connectionId: 'cid',
    ownerLeaseId: 'lease',
    connectionGeneration: 'cg',
    registerDatabase() {}
  }
  const database = IpcGattDatabase.fromPayload(manager, connection, {
    schemaVersion: 2,
    handle: 'database',
    databaseId: 'did',
    databaseGeneration: 'dg',
    services: [{ uuid: serviceUuid, occurrence: '0', primary: true, includedServices: [] }],
    characteristics: [
      {
        handle: 'ch',
        serviceUuid,
        serviceOccurrence: '0',
        characteristicUuid,
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
  expect(setup.state).toBe('ready')
  const iterator = setup.stream[Symbol.asyncIterator]()
  await expect(iterator.next()).resolves.toMatchObject({
    value: {
      memberId: 'A',
      collectionId: '7',
      sourceGeneration: { kind: 'counter', value: '18446744073709551615' },
      ordinal: '18446744073709551615',
      value: Uint8Array.of(0, 255)
    }
  })
  await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'terminal' } })
  await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
  expect(
    calls.filter(call => call.command === 'gatt.ordered.next').some(call => call.payload.ackDeliveryId === '1')
  ).toBe(true)
  await expect(setup.stream.close()).resolves.toMatchObject({ group: { state: 'released' } })
})

test('public IPC marker and terminal rejection preserve exact identity', () => {
  const { decodeOrderedItem, decodePublicMarkerWriteStatus } = require('../../src/ipc/ordered-codec')
  const marker = decodeOrderedItem({
    kind: 'marker',
    collectionId: '4',
    connectionGeneration: 'c',
    databaseGeneration: 'd',
    markerId: '5',
    ordinal: '6',
    sourceGeneration: null,
    sourceMembership: null
  })
  expect(marker).toMatchObject({ kind: 'marker', collectionId: '4', markerId: '5', ordinal: '6' })
  const terminal = decodeOrderedItem({
    kind: 'terminal',
    reason: 'closed',
    uncertainTail: true,
    sourceRejection: { kind: 'marker', markerId: '5' }
  })
  expect(terminal.sourceRejection).toEqual({ kind: 'marker', markerId: '5' })
  expect(decodePublicMarkerWriteStatus({ state: 'retired' })).toEqual({ state: 'retired' })
  expect(
    decodePublicMarkerWriteStatus({ state: 'admitted', ordinal: '6', write: { state: 'initiating' } })
  ).toMatchObject({ state: 'admitted', write: { state: 'initiating' } })
})

test('ordered terminal codecs retain unknown omitted failure evidence', () => {
  const { decodeOrderedNativeItem } = require('../../src/core/ordered-native-wire')
  const { decodeOrderedItem, decodeOrderedCleanup } = require('../../src/ipc/ordered-codec')
  const wire = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    uncertainTail: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: {
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'source.late',
      detail: 'failure',
      platformDomain: 'android',
      platformCode: '257',
      platformMessage: null,
      platformMetadata: {}
    },
    additionalFailureCount: '2',
    failureSummaryVersion: 1,
    failureCountOverflowed: false,
    failureObservationsOmitted: true
  }
  const context = {
    collectionId: '1',
    connectionGeneration: 'c',
    databaseGeneration: 'd',
    members: [],
    byteCapacity: 8,
    lastOrdinal: null
  }
  const terminal = decodeOrderedNativeItem(wire, context)
  expect(terminal.secondaryError).toMatchObject({ code: 'platform.failure', operation: 'source.late' })
  expect(terminal.additionalFailureCount).toBe('2')
  expect(terminal.failureObservationsOmitted).toBe(true)
  expect(decodeOrderedItem(terminal).failureObservationsOmitted).toBe(true)
  const cleanup = decodeOrderedCleanup({
    group: { state: 'released', failures: [] },
    members: [],
    discarded: { values: '0', bytes: '0', controls: '1', uncertainTail: true, terminal }
  })
  expect(cleanup.discarded.terminal).toMatchObject({ additionalFailureCount: '2', failureObservationsOmitted: true })
  expect(() => decodeOrderedNativeItem({ ...wire, failureSummaryVersion: 2 }, context)).toThrow()
  const { failureSummaryVersion, failureCountOverflowed, failureObservationsOmitted, ...legacy } = wire
  expect(() => decodeOrderedNativeItem(legacy, context)).toThrow()
  expect(() => decodeOrderedNativeItem({ ...wire, failureObservationsOmitted: undefined }, context)).toThrow()
})

test('ordered recording decoder preserves versioned failure summary and legacy bytes', () => {
  const { parseOrderedRecordingBatch } = require('../../src/core/ordered-recording')
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: {
      code: 'platform.failure',
      domain: 'gatt',
      operation: 'source.late',
      detail: 'failure',
      platformDomain: 'android',
      platformCode: '257',
      platformMessage: null,
      platformMetadata: {}
    },
    additionalFailureCount: '2',
    failureSummaryVersion: 1,
    failureCountOverflowed: false,
    failureObservationsOmitted: true
  }
  const decode = record => {
    const row = {
      ordinal: 1,
      metadata: {
        kind: 'ordered',
        version: 1,
        recordingId: 'r',
        peerId: 'p',
        collectionId: '1',
        sourceGeneration: { kind: 'counter', value: '1' },
        deliveryId: '1'
      },
      record
    }
    const bytes = Buffer.byteLength(JSON.stringify(row))
    return parseOrderedRecordingBatch({ token: 't', records: [row], bytes, more: false }, 'r', {
      maxItems: 1,
      maxBytes: bytes
    }).records[0].record
  }
  expect(decode(terminal)).toEqual(terminal)
  const { failureSummaryVersion, failureCountOverflowed, failureObservationsOmitted, ...legacy } = terminal
  expect(decode(legacy)).toEqual(legacy)
  expect(() => decode({ ...terminal, failureSummaryVersion: 2 })).toThrow()
})
