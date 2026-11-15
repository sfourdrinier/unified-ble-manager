// __tests__/backends/reactnative/ordered-recording-controls.test.js
const { createSessionOrderedRecordingControls } = require('../../../src/backends/reactnative/react-native-ordered-recording-controls')

function session(version = 1) {
  const context = { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: '1', databaseGeneration: '2', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'a', selector: { serviceUuid: 'service', serviceOccurrence: '0', characteristicUuid: 'char', characteristicOccurrence: '0' }, delivery: null, preference: 'notification' }] }
  return { orderedRecordingVersion: jest.fn(async () => version), orderedRecordingStart: jest.fn(), orderedRecordingReserve: jest.fn(), orderedRecordingStartStatus: jest.fn(), orderedRecordingRetire: jest.fn(), orderedRecordingContext: jest.fn(async () => JSON.stringify({ ok: true, value: context })), orderedRecordingStop: jest.fn(async () => JSON.stringify({ ok: true, value: { recording: { version: 1, state: 'waiting-drain', error: null }, hostCleanup: { state: 'not-applicable' } } })) }
}

test('live ordered context and stop use session-scoped native controls and preserve pending outcome', async () => {
  const native = session()
  const controls = createSessionOrderedRecordingControls(native)
  expect((await controls.context('recording')).members[0].publicMemberId).toBe('a')
  expect(await controls.stop('recording')).toEqual({ recording: { version: 1, state: 'waiting-drain', error: null }, hostCleanup: { state: 'not-applicable' } })
  expect(native.orderedRecordingContext).toHaveBeenCalledWith('recording')
  expect(native.orderedRecordingStop).toHaveBeenCalledWith('recording')
})

test('incompatible version refuses before native context or stop', async () => {
  const native = session(0)
  const controls = createSessionOrderedRecordingControls(native)
  await expect(controls.stop('recording')).rejects.toThrow()
  expect(native.orderedRecordingStop).not.toHaveBeenCalled()
})
