const { createReactNativeContinuationRecordings } = require('../../../src/react-native-continuation-recording')

test('mobile recording access uses identity-checked binding without a manager or radio session', async () => {
  class Binding {
    id = 'h10'
    openSession = jest.fn(() => {
      throw new Error('radio permission denied')
    })
    async continuationRecordingPrepare(id) {
      expect(id).toBe(this.id)
      return JSON.stringify({ ok: true, value: { token: null, records: [], bytes: 0, more: false } })
    }
  }
  const binding = new Binding()
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  expect(await recordings.prepare('h10', { maxItems: 10, maxBytes: 4096 })).toEqual({
    token: null,
    records: [],
    bytes: 0,
    more: false
  })
  expect(binding.openSession).not.toHaveBeenCalled()
})

test('an older or injected binding without recording controls refuses explicitly', async () => {
  const recordings = createReactNativeContinuationRecordings({ rustCore: {} })
  await expect(recordings.status('h10')).rejects.toMatchObject({ code: 'capability.unsupported' })
  await expect(recordings.stop('h10')).rejects.toMatchObject({ code: 'capability.unsupported' })
})

test('unsupported default host throws the public typed error without attempting native access', () => {
  expect(() => createReactNativeContinuationRecordings()).toThrow(BleError)
})
jest.mock('react-native', () => ({ Platform: { OS: 'web' }, TurboModuleRegistry: { get: jest.fn() } }))
const { BleError } = require('../../../src/public/errors')


test('recovery hint subscribes without a native session and detaches independently', () => {
  const listeners = new Set()
  const binding = {
    openSession: jest.fn(),
    onRecoveryHint: listener => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    }
  }
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  const listener = jest.fn()
  const remove = recordings.onRecoveryHint(listener)
  for (const callback of listeners) callback()
  expect(listener).toHaveBeenCalledTimes(1)
  remove()
  for (const callback of listeners) callback()
  expect(listener).toHaveBeenCalledTimes(1)
  expect(binding.openSession).not.toHaveBeenCalled()
})

test('missing recovery hint capability fails explicitly', () => {
  const recordings = createReactNativeContinuationRecordings({ rustCore: {} })
  expect(() => recordings.onRecoveryHint(() => {})).toThrow(BleError)
})

test('terminal evidence recovery lists, reads and explicitly acknowledges native-selected opaque references', async () => {
  const binding = {
    terminalEvidenceRecoveryOpenSelected: jest.fn(async () =>
      JSON.stringify({ ok: true, value: { leaseId: 'lease-1' } })
    ),
    terminalEvidenceRecoveryList: jest.fn(async (leaseId, maxItems, maxBytes) => {
      expect(leaseId).toBe('lease-1')
      expect(maxItems).toBe(1)
      expect(maxBytes).toBe(64)
      return JSON.stringify({
        ok: true,
        value: {
          items: [
            {
              handle: 'opaque-retained-ref',
              producer: 'mobile-owner',
              scope: { managerId: 'native-manager', operationId: 'scan-terminal', incarnation: '7' },
              state: 'committed',
              reference: 'retained-ref-7',
              byteLength: '3',
              digest: 'a'.repeat(64),
              formatVersion: 1
            }
          ],
          more: false
        }
      })
    }),
    terminalEvidenceRecoveryListWithStatus: jest.fn(async (leaseId, maxItems, maxBytes) => {
      expect(leaseId).toBe('lease-1')
      expect(maxItems).toBe(1)
      expect(maxBytes).toBe(64)
      return JSON.stringify({
        ok: true,
        value: {
          items: [],
          more: false,
          busyProducers: ['scan']
        }
      })
    }),
    terminalEvidenceRecoveryRead: jest.fn(async (handle, offset, maxBytes) => {
      expect(handle).toBe('opaque-retained-ref')
      expect(offset).toBe('0')
      expect(maxBytes).toBe(3)
      return JSON.stringify({ ok: true, value: { bytesBase64: 'AQID', more: false } })
    }),
    terminalEvidenceRecoveryAcknowledge: jest.fn(async handle => {
      expect(handle).toBe('opaque-retained-ref')
      return JSON.stringify({ ok: true, value: {} })
    })
  }
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  const page = await recordings.terminalEvidenceRecovery.list({ maxItems: 1, maxBytes: 64 })
  expect(page.items).toEqual([
    expect.objectContaining({ handle: 'opaque-retained-ref', reference: 'retained-ref-7', byteLength: 3 })
  ])
  await expect(recordings.terminalEvidenceRecovery.listWithStatus({ maxItems: 1, maxBytes: 64 })).resolves.toEqual({
    items: [],
    more: false,
    busyProducers: ['scan']
  })
  const chunk = await recordings.terminalEvidenceRecovery.read('opaque-retained-ref', { offset: 0, maxBytes: 3 })
  expect(chunk).toEqual({ offset: 0, nextOffset: 3, bytes: new Uint8Array([1, 2, 3]), done: true })
  expect(binding.terminalEvidenceRecoveryAcknowledge).not.toHaveBeenCalled()
  await recordings.terminalEvidenceRecovery.acknowledge('opaque-retained-ref')
  expect(binding.terminalEvidenceRecoveryOpenSelected).toHaveBeenCalledTimes(1)
})

test('terminal evidence recovery carries a staged entry through exact read and native promotion acknowledgement', async () => {
  const binding = {
    terminalEvidenceRecoveryOpenSelected: jest.fn(async () => JSON.stringify({ ok: true, value: { leaseId: 'lease-staged' } })),
    terminalEvidenceRecoveryList: jest.fn(async () =>
      JSON.stringify({
        ok: true,
        value: {
          items: [{
            handle: 'opaque-staged-entry',
            producer: 'scan',
            scope: { managerId: 'native-manager', operationId: 'scan-terminal', incarnation: '8' },
            state: 'staged'
          }],
          more: false
        }
      })
    ),
    terminalEvidenceRecoveryRead: jest.fn(async () =>
      JSON.stringify({ ok: true, value: { bytesBase64: 'BAUG', more: false } })
    ),
    terminalEvidenceRecoveryAcknowledge: jest.fn(async handle => {
      expect(handle).toBe('opaque-staged-entry')
      return JSON.stringify({ ok: true, value: {} })
    })
  }
  const recovery = createReactNativeContinuationRecordings({ rustCore: binding }).terminalEvidenceRecovery
  const page = await recovery.list({ maxItems: 1, maxBytes: 64 })
  expect(page.items).toEqual([expect.objectContaining({ handle: 'opaque-staged-entry', state: 'staged' })])
  await expect(recovery.read('opaque-staged-entry', { offset: 0, maxBytes: 3 })).resolves.toMatchObject({
    bytes: new Uint8Array([4, 5, 6]),
    done: true
  })
  await expect(recovery.acknowledge('opaque-staged-entry')).resolves.toBeUndefined()
})

test('offline ordered context uses only the dedicated trusted native control', async () => {
  const context = { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: '7', databaseGeneration: '8', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'a', selector: { serviceUuid: 'service', serviceOccurrence: '0', characteristicUuid: 'characteristic', characteristicOccurrence: '0' }, delivery: null, preference: 'notification' }] }
  const binding = { orderedRecordingOfflineContext: jest.fn(async id => JSON.stringify({ ok: true, value: context })) }
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  expect(await recordings.contextOrdered('recording')).toEqual(context)
  expect(binding.orderedRecordingOfflineContext).toHaveBeenCalledWith('recording')
  await expect(recordings.stopOrdered('recording')).rejects.toMatchObject({ code: 'capability.unsupported' })
})

test('explicit offline ordered status and stop preserve interruption and refuse a live owner', async () => {
  const snapshot = { recordingId: 'h10', phase: 'stopped', accepting: false, records: 0, bytes: 0, lostRecords: 0, maxBytes: 1048576, maxRecords: 100, encrypted: false, runtimeFailure: null, collectionFailure: null, orderedOwner: { version: 1, producerIncarnation: 'p', collectionId: '4', sourceGeneration: null, state: 'interrupted', uncertainTail: true } }
  const binding = {
    orderedRecordingOfflineStatus: jest.fn(async () => JSON.stringify({ ok: true, value: { version: 1, state: 'interrupted', journal: snapshot } })),
    orderedRecordingOfflineStop: jest.fn(async () =>
      JSON.stringify({
        ok: true,
        value: {
          recording: { version: 2, state: 'interrupted', error: null },
          hostCleanup: { state: 'uncertain' }
        }
      })
    )
  }
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  expect((await recordings.statusOrderedOffline('h10')).journal.orderedOwner).toMatchObject({ state: 'interrupted', uncertainTail: true })
  expect(await recordings.stopOrdered('h10')).toEqual({
    recording: { version: 2, state: 'interrupted', error: null },
    hostCleanup: { state: 'uncertain' }
  })
  binding.orderedRecordingOfflineStop.mockResolvedValue(JSON.stringify({ ok: false, error: { code: 'ownership.denied', domain: 'restoration', operation: 'ordered.offline.stop', detail: null }, commit: null, retryability: 'never' }))
  await expect(recordings.stopOrdered('h10')).rejects.toMatchObject({ code: 'ownership.denied' })
})


test('offline ordered status rejects an inconsistent owner', async () => {
  const journal = { recordingId: 'h10', phase: 'stopped', accepting: false, records: 0, bytes: 0, lostRecords: 0, maxBytes: 1048576, maxRecords: 100, encrypted: false, runtimeFailure: null, collectionFailure: null }
  const binding = { orderedRecordingOfflineStatus: jest.fn(async () => JSON.stringify({ ok: true, value: { version: 1, state: 'sealed', journal } })) }
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  await expect(recordings.statusOrderedOffline('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
  binding.orderedRecordingOfflineStatus.mockResolvedValue(JSON.stringify({ ok: true, value: { version: 1, state: 'not-started', journal } }))
  expect((await recordings.statusOrderedOffline('h10')).state).toBe('not-started')
})
