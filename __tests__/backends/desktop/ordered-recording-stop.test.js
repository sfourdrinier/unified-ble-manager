// __tests__/backends/desktop/ordered-recording-stop.test.js
const { createNativeContinuationRecordingController } = require('../../../src/core/continuation-recording')

const envelope = value => JSON.stringify({ ok: true, value })
const stopEnvelope = recording => envelope({ recording, hostCleanup: { state: 'not-applicable' } })
const base = stopOrdered => ({ status: jest.fn(), prepare: jest.fn(), acknowledge: jest.fn(), stop: jest.fn(), clear: jest.fn(), orderedReceiptVersion: async () => 1, stopOrdered })

test('ordered stop retries pending ownership and preserves interrupted and failure evidence', async () => {
  const transport = jest.fn()
    .mockResolvedValueOnce(stopEnvelope({ version: 1, state: 'waiting-drain', error: null }))
    .mockResolvedValueOnce(stopEnvelope({ version: 1, state: 'finished', error: null }))
    .mockResolvedValueOnce(stopEnvelope({ version: 1, state: 'interrupted', error: null }))
    .mockResolvedValueOnce(stopEnvelope({ version: 1, state: 'failed', error: { kind: 'missing-terminal' } }))
  const controller = createNativeContinuationRecordingController(base(transport), 'ubm-desktop')
  expect((await controller.stopOrdered('recording')).recording.state).toBe('waiting-drain')
  expect((await controller.stopOrdered('recording')).recording.state).toBe('finished')
  expect((await controller.stopOrdered('recording')).recording.state).toBe('interrupted')
  expect((await controller.stopOrdered('recording')).recording.error).toEqual({ kind: 'missing-terminal' })
  expect(transport).toHaveBeenCalledTimes(4)
})

test.each([
  { version: 1, state: 'unknown', error: null },
  { version: 2, state: 'finished', error: null },
  { version: 1, state: 'finished', error: { kind: 'missing-terminal' } },
  { version: 1, state: 'failed', error: { kind: 'worker-failed', extra: true } }
])('rejects malformed ordered stop receipt %#', async receipt => {
  const controller = createNativeContinuationRecordingController(base(async () => stopEnvelope(receipt)), 'ubm-desktop')
  await expect(controller.stopOrdered('recording')).rejects.toThrow('protocol.malformed')
})

test('retains structured storage and source cleanup errors', async () => {
  const source = { code: 'connection.lost', domain: 'connection', operation: 'release', detail: null, platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} }
  const storage = { kind: 'storage.io', detail: 'failed', operation: 'append', sqliteExtendedCode: 10, sqliteCode: 'SQLITE_IOERR' }
  const receipt = { version: 1, state: 'failed', error: { kind: 'storage-cleanup', source, storage } }
  const controller = createNativeContinuationRecordingController(base(async () => stopEnvelope(receipt)), 'ubm-desktop')
  expect((await controller.stopOrdered('recording')).recording).toEqual(receipt)
})

test('contextOrdered validates the persisted member mapping', async () => {
  const context = { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'a', databaseGeneration: 'b', maxItems: 8, maxBytes: 100, maxControls: 2, members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] }
  const access = base(async () => envelope({ version: 1, state: 'finished', error: null }))
  access.orderedContext = async () => envelope(context)
  const controller = createNativeContinuationRecordingController(access, 'ubm-desktop')
  expect(await controller.contextOrdered('recording')).toEqual(context)
  context.members[0].memberId = '00'
  await expect(controller.contextOrdered('recording')).rejects.toThrow('protocol.malformed')
})

test('ordered stop preserves host cleanup failure and pending state', async () => {
  const recording = { version: 1, state: 'finished', error: null }
  const error = { code: 'connection.lost', domain: 'connection', operation: 'connection.release', platform: null, retryability: 'caller-decides' }
  const access = base(async () => envelope({ recording, hostCleanup: { state: 'release-failed', failures: [{ resourceKind: 'lease', error }] } }))
  const controller = createNativeContinuationRecordingController(access, 'ubm-desktop')
  expect(await controller.stopOrdered('recording')).toEqual({ recording, hostCleanup: { state: 'release-failed', failures: [{ resourceKind: 'lease', error }] } })
  access.stopOrdered = async () => envelope({ recording, hostCleanup: { state: 'pending' } })
  const retry = createNativeContinuationRecordingController(access, 'ubm-desktop')
  expect((await retry.stopOrdered('recording')).hostCleanup).toEqual({ state: 'pending' })
})

test('ordered stop preserves uncertain host cleanup after interrupted release', async () => {
  const recording = { version: 1, state: 'interrupted', error: null }
  const controller = createNativeContinuationRecordingController(base(async () => envelope({ recording, hostCleanup: { state: 'uncertain' } })), 'ubm-desktop')
  expect(await controller.stopOrdered('recording')).toEqual({ recording, hostCleanup: { state: 'uncertain' } })
})
