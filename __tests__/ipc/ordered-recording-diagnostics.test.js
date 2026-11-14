// __tests__/ipc/ordered-recording-diagnostics.test.js
const { createDesktopOrderedDiagnostics } = require('../../src/ipc/ordered-diagnostics')
const { parseOrderedRecordingHostCleanup } = require('../../src/core/continuation-recording')

test('interrupted host lease outcome remains uncertain', () => {
  expect(parseOrderedRecordingHostCleanup({ state: 'uncertain' })).toEqual({ state: 'uncertain' })
})

test('recording recovery pages and retires authenticated start handles', async () => {
  const calls = []
  const client = { request: async request => {
    calls.push(request)
    if (request.command === 'gatt.ordered-recording.starts') return { payload: request.payload.after === null
      ? { starts: [{ startHandle: '3', recordingId: 'one', state: 'result' }], truncated: true, nextCursor: '3' }
      : { starts: [{ startHandle: '5', recordingId: 'two', state: 'retired' }], truncated: false, nextCursor: null } }
    if (request.command === 'gatt.ordered-recording.status') return { payload: { state: 'pending' } }
    if (request.command === 'gatt.ordered-recording.retire') return { payload: { retired: true } }
    throw new Error('unexpected command')
  } }
  const diagnostics = createDesktopOrderedDiagnostics(client)
  const first = await diagnostics.recordingStarts(null, 1)
  expect(first.nextCursor).toBe('3')
  const second = await diagnostics.recordingStarts(first.nextCursor, 1)
  expect(second.starts[0]).toEqual({ startHandle: '5', recordingId: 'two', state: 'retired' })
  expect(await diagnostics.recordingStartStatus('3', 'one', 1)).toEqual({ state: 'pending' })
  await diagnostics.retireRecordingStart('3')
  expect(calls.map(call => call.command)).toEqual(['gatt.ordered-recording.starts', 'gatt.ordered-recording.starts', 'gatt.ordered-recording.status', 'gatt.ordered-recording.retire'])
})

test('orphan recovery preserves marker and write outcome without payloads', async () => {
  const receipt = { format: 'public', receiptId: 'r', values: '0', bytes: '0', controls: '1', uncertainTail: false, terminal: null, losses: [], markers: [{ kind: 'marker', collectionId: 'c', connectionGeneration: 'g', databaseGeneration: 'd', markerId: '0', ordinal: '1' }], markerWrites: [{ markerHandle: '2', statusFormat: 'public', status: { state: 'admitted', ordinal: '1', write: { state: 'settled', error: null } } }] }
  const encoded = JSON.stringify(receipt)
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ payload }) => ({ payload: payload.offset === 0
    ? { chunk: encoded, nextOffset: Array.from(encoded).length, done: false }
    : { chunk: '', nextOffset: payload.offset, done: true } }) })
  const recovered = await diagnostics.read('r')
  expect(recovered.markers[0]).toMatchObject({ kind: 'marker', markerId: '0', ordinal: '1' })
  expect(recovered.markerWrites[0]).toMatchObject({ markerHandle: '2', status: { state: 'admitted', write: { state: 'settled' } } })
})

test('orphan diagnostics retain separate v2 failure observations and reject unrecognized evidence', async () => {
  const observation = { kind: 'failure-observation', failure: null, sourceRejection: { kind: 'fence', membership: { member: '3', epoch: '4' } }, sourceFailureMember: null, rejectedLoss: null, tailUncertain: true }
  const receipt = { format: 'public', receiptVersion: 2, receiptId: 'r', values: '0', bytes: '0', controls: '3', uncertainTail: true, terminal: null, losses: [], failureObservations: [observation, observation], failureObservationsOmitted: [{ kind: 'failure-observations-omitted', count: null }] }
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ payload }) => {
    const encoded = JSON.stringify(receipt)
    return { payload: payload.offset === 0 ? { chunk: encoded, nextOffset: Array.from(encoded).length, done: false } : { chunk: '', nextOffset: payload.offset, done: true } }
  } })
  expect(await diagnostics.read('r')).toMatchObject({ receiptVersion: 2, failureObservations: [observation, observation], failureObservationsOmitted: [{ kind: 'failure-observations-omitted', count: null }] })
  receipt.receiptVersion = 3
  await expect(diagnostics.read('r')).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  receipt.receiptVersion = 2
  receipt.unrecognizedEvidence = { important: true }
  await expect(diagnostics.read('r')).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('native v2 orphan diagnostics map member evidence without merging observations', async () => {
  const nativeStatus = JSON.stringify({ members: [{ memberId: '0', acquisition: 'acquired', effectiveDelivery: 'notification', cleanupReleased: true, cleanupError: null }], sourceGeneration: null, setupError: null, fenceError: null })
  const observation = { kind: 'failure-observation', failure: null, sourceRejection: null, sourceFailureMember: null, rejectedLoss: { scope: { kind: 'member', memberId: '0' }, unit: 'values', count: '4', bytes: null }, tailUncertain: true }
  const receipt = { format: 'native', receiptVersion: 2, receiptId: 'n', values: '0', bytes: '0', controls: '2', uncertainTail: true, terminal: null, losses: [], memberIds: ['sensor'], nativeStatus, collectionId: '1', connectionGeneration: 'c', databaseGeneration: 'd', failureObservations: [observation, observation], failureObservationsOmitted: [] }
  const encoded = JSON.stringify(receipt)
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ payload }) => ({ payload: payload.offset === 0 ? { chunk: encoded, nextOffset: encoded.length, done: false } : { chunk: '', nextOffset: payload.offset, done: true } }) })
  const result = await diagnostics.read('n')
  expect(result.failureObservations).toHaveLength(2)
  expect(result.failureObservations[0].rejectedLoss.scope).toEqual({ kind: 'member', memberId: 'sensor' })
  expect(result.failureObservations[1]).toEqual(result.failureObservations[0])
  expect(JSON.parse(result.wireReceipt).nativeStatus).toEqual(nativeStatus)
})

test('an unread or malformed receipt cannot be acknowledged by diagnostics', async () => {
  let ackCalls = 0
  const receipt = JSON.stringify({ format: 'public', receiptVersion: 3, receiptId: 'r', values: '0', bytes: '0', controls: '0', uncertainTail: false, terminal: null, losses: [] })
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ command, payload }) => {
    if (command === 'gatt.ordered.orphan-ack') { ackCalls++; return { payload: { state: 'forgotten' } } }
    return { payload: payload.offset === 0 ? { chunk: receipt, nextOffset: receipt.length, done: false } : { chunk: '', nextOffset: payload.offset, done: true } }
  } })
  await expect(diagnostics.acknowledge('r')).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  await expect(diagnostics.read('r')).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  await expect(diagnostics.acknowledge('r')).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  expect(ackCalls).toBe(0)
})

test('revoked reclaim cleanup preserves paging and retries lost final confirmation', async () => {
  const calls = []
  let loseConfirmation = true
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ command }) => {
    calls.push(command)
    if (command.endsWith('.pending')) return { payload: { transactions: [{ reclaimHandle: '7', state: 'revoked' }], truncated: false, nextCursor: null } }
    if (command.endsWith('.status')) return { payload: { status: { state: 'released', mappingCommitted: true, error: null } } }
    if (command.endsWith('.retire')) return { payload: { retired: true } }
    if (command.endsWith('.ack-retirement')) return { payload: { acknowledged: true } }
    if (command.endsWith('.forget-retirement')) return { payload: { forgotten: true } }
    if (command.endsWith('.confirm-forgotten')) {
      if (loseConfirmation) { loseConfirmation = false; throw new Error('lost confirmation') }
      return { payload: { confirmed: true } }
    }
    throw new Error('unexpected command')
  } })
  expect((await diagnostics.revokedRecordingReclaims()).transactions).toEqual([{ reclaimHandle: '7', state: 'revoked' }])
  expect((await diagnostics.reclaimCleanup('7', 'status')).state).toBe('released')
  await expect(diagnostics.retireRevokedReclaim('7')).rejects.toThrow('lost confirmation')
  await diagnostics.retireRevokedReclaim('7')
  expect(calls.filter(command => command.endsWith('.retire'))).toHaveLength(1)
  expect(calls.filter(command => command.endsWith('.confirm-forgotten'))).toHaveLength(2)
})

test('native v2 orphan revision reaches observation and marker failure decoders', async () => {
  const failure = { code: 'platform.failure', domain: 'gatt', operation: 'source', detail: null, platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {}, platformCleanupFailures: [{ code: 'platform.failure', domain: 'gatt', operation: 'stop', detail: null, platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} }] }
  const observation = { kind: 'failure-observation', failure, sourceRejection: null, sourceFailureMember: null, rejectedLoss: null, tailUncertain: true }
  const receipt = { format: 'native', receiptVersion: 2, receiptId: 'n', values: '0', bytes: '0', controls: '1', uncertainTail: true, terminal: null, losses: [], memberIds: [], nativeStatus: JSON.stringify({ members: [], sourceGeneration: null, setupError: null, fenceError: null }), collectionId: '1', connectionGeneration: 'c', databaseGeneration: 'd', failureObservations: [observation], failureObservationsOmitted: [{ kind: 'failure-observations-omitted', count: null }], markerWrites: [{ markerHandle: '2', statusFormat: 'native', status: { state: 'rejected', error: failure } }] }
  const encoded = JSON.stringify(receipt)
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ payload }) => ({ payload: payload.offset === 0 ? { chunk: encoded, nextOffset: encoded.length, done: false } : { chunk: '', nextOffset: payload.offset, done: true } }) })
  const result = await diagnostics.read('n')
  expect(result.failureObservations[0].failure.orderedFailure).toEqual(failure)
  expect(result.markerWrites[0].status.error.orderedFailure).toEqual(failure)
  expect(result.failureObservationsOmitted).toEqual(receipt.failureObservationsOmitted)
  expect(result.wireReceipt).toBe(encoded)
})

test('native orphan status refuses unknown nested evidence before acknowledgement', async () => {
  const status = { members: [], sourceGeneration: null, setupError: null, fenceError: null, futureEvidence: 'retain' }
  const receipt = { format: 'native', receiptVersion: 2, receiptId: 'n', values: '0', bytes: '0', controls: '0', uncertainTail: false, terminal: null, losses: [], memberIds: [], nativeStatus: JSON.stringify(status), collectionId: '1', connectionGeneration: 'c', databaseGeneration: 'd', failureObservations: [], failureObservationsOmitted: [] }
  const encoded = JSON.stringify(receipt)
  let acknowledgements = 0
  const diagnostics = createDesktopOrderedDiagnostics({ request: async ({ command, payload }) => {
    if (command === 'gatt.ordered.orphan-ack') { acknowledgements++; return { payload: { state: 'forgotten' } } }
    return { payload: payload.offset === 0 ? { chunk: encoded, nextOffset: encoded.length, done: false } : { chunk: '', nextOffset: payload.offset, done: true } }
  } })
  await expect(diagnostics.read('n')).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  await expect(diagnostics.acknowledge('n')).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  expect(acknowledgements).toBe(0)
})

test('encoded ordered failure rejects conflicting, malformed and oversized evidence', () => {
  const { decodeOrderedItem } = require('../../src/ipc/ordered-codec')
  const base = { kind: 'failure-observation', failure: { code: 'platform.failure', domain: 'gatt', operation: 'source', platform: null, retryability: 'never', orderedFailureJson: '{' }, sourceRejection: null, sourceFailureMember: null, rejectedLoss: null, tailUncertain: true }
  expect(() => decodeOrderedItem(base)).toThrow()
  expect(() => decodeOrderedItem({ ...base, failure: { ...base.failure, orderedFailureJson: 'x'.repeat(1048577) } })).toThrow()
  expect(() => decodeOrderedItem({ ...base, failure: { ...base.failure, orderedFailure: {} } })).toThrow()
})

test('ordered cleanup rejects unknown nested failure evidence', () => {
  const { decodeOrderedCleanup } = require('../../src/ipc/ordered-codec')
  const error = { code: 'platform.failure', domain: 'gatt', operation: 'cleanup', platform: null, retryability: 'never', futureEvidence: true }
  expect(() => decodeOrderedCleanup({ group: { state: 'release-failed', failures: [{ resourceKind: 'group', error }] }, members: [] })).toThrow()
})
