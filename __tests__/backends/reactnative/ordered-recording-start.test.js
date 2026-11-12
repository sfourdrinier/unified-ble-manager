// __tests__/backends/reactnative/ordered-recording-start.test.js
const { createReactNativeOrderedRecordingStart } = require('../../../src/backends/reactnative/react-native-ordered-recording-start')
beforeAll(async () => {
  await require('../../../src/backends/reactnative/react-native-reservation-key').initializeReactNativeReservationKeys(async length => new Uint8Array(length).fill(7))
})
const envelope = value => JSON.stringify({ ok: true, value })
const context = { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: '1', databaseGeneration: '2', maxItems: 8, maxBytes: 1024, maxControls: 4, members: [{ memberId: '0', publicMemberId: 'a', selector: { serviceUuid: 'service', serviceOccurrence: '0', characteristicUuid: 'char', characteristicOccurrence: '0' }, delivery: null, preference: 'notification' }] }
const receipt = { kind: 'started', receipt: { version: 1, recordingId: 'r', collectionId: '18446744073709551615', state: 'active', sourceGeneration: null, members: [{ memberId: '0', acquisition: 'acquired', delivery: 'notification', cleanupReleased: false, cleanupError: null }], setupFailure: null, fenceFailure: null, error: null, inspectionError: null, driverError: null } }
const wireReceipt = { ...receipt, reclaimCapability: 'private-capability' }
function session() { return { orderedRecordingVersion: jest.fn(async () => 1), orderedRecordingReserve: jest.fn(async () => envelope({ version: 2, state: 'reserved', handle: 'native-handle' })), orderedRecordingStart: jest.fn(async () => envelope(wireReceipt)), orderedRecordingStartStatus: jest.fn(async () => envelope({ version: 1, state: 'ready', receipt: wireReceipt })), orderedRecordingRetire: jest.fn(async () => envelope({ version: 1, state: 'retired' })), orderedRecordingContext: jest.fn(), orderedRecordingStop: jest.fn() } }
const input = { id: 'r', maxBytes: 1048576, maxRecords: 100, declaration: context, budgetMs: 5000, refreshBudget: () => 5000, signal: null }

test('version checked before reservation; retries preserve exact owner identity', async () => {
  const native = session(); const handle = createReactNativeOrderedRecordingStart(native)
  expect(await handle.start(input)).toEqual(receipt)
  expect(await handle.status()).toEqual({ state: 'result', result: receipt })
  expect(native.orderedRecordingStart.mock.calls[0].slice(0, 4)).toEqual(['native-handle', 'r', '1048576', '100'])
  await expect(handle.start({ ...input, maxRecords: 101 })).rejects.toThrow()
  await handle.retire()
  expect(native.orderedRecordingRetire).toHaveBeenCalledWith('native-handle')
})

test('incompatible version cannot reserve or start', async () => {
  const native = session(); native.orderedRecordingVersion.mockResolvedValue(0)
  const handle = createReactNativeOrderedRecordingStart(native)
  await expect(handle.start(input)).rejects.toThrow()
  expect(native.orderedRecordingReserve).not.toHaveBeenCalled()
  expect(native.orderedRecordingStart).not.toHaveBeenCalled()
})

test('lost start reply is recovered from session-owned status', async () => {
  const native = session(); native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  const handle = createReactNativeOrderedRecordingStart(native)
  await expect(handle.start(input)).rejects.toThrow()
  expect(await handle.status()).toEqual({ state: 'result', result: receipt })
  await handle.retire()
})

test('retire reconciles a lost ready reply before deleting its receipt', async () => {
  const native = session(); native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  const retain = jest.fn(); const handle = createReactNativeOrderedRecordingStart(native, retain)
  await expect(handle.start(input)).rejects.toThrow('lost reply')
  await handle.retire()
  expect(retain).toHaveBeenCalledWith('18446744073709551615', 'private-capability')
  expect(await handle.status()).toEqual({ state: 'result', result: receipt })
})

test.each(['failed', 'retired'])('reports native %s start status', async state => {
  const native = session(); native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  native.orderedRecordingStartStatus.mockResolvedValue(envelope({ version: 1, state, receipt: null }))
  const handle = createReactNativeOrderedRecordingStart(native)
  await expect(handle.start(input)).rejects.toThrow('lost reply')
  expect(await handle.status()).toEqual({ state })
})

test('retire during reservation prevents later start and retires the native receipt', async () => {
  const native = session(); let resolveReserve
  native.orderedRecordingReserve.mockImplementation(() => new Promise(resolve => { resolveReserve = resolve }))
  const handle = createReactNativeOrderedRecordingStart(native)
  const start = handle.start(input)
  await Promise.resolve(); await Promise.resolve()
  const retiring = handle.retire()
  resolveReserve(envelope({ version: 2, state: 'reserved', handle: 'native-handle' }))
  await retiring
  await expect(start).rejects.toThrow()
  expect(native.orderedRecordingStart).not.toHaveBeenCalled()
  expect(native.orderedRecordingRetire).toHaveBeenCalledWith('native-handle')
})

test('expired deadline after reservation refuses dispatch and leaves receipt available for retirement', async () => {
  const native = session(); const handle = createReactNativeOrderedRecordingStart(native)
  await expect(handle.start({ ...input, refreshBudget: () => { throw new Error('deadline') } })).rejects.toThrow('deadline')
  expect(native.orderedRecordingStart).not.toHaveBeenCalled()
  await handle.retire()
  expect(native.orderedRecordingRetire).toHaveBeenCalledWith('native-handle')
})

test('lost reserve response retries the same recording and retains its issued handle', async () => {
  const native = session()
  native.orderedRecordingReserve.mockRejectedValueOnce(new Error('lost response'))
  const handle = createReactNativeOrderedRecordingStart(native)
  await expect(handle.start(input)).rejects.toThrow('lost response')
  expect(await handle.status()).toEqual({ state: 'unknown' })
  expect(await handle.start(input)).toEqual(receipt)
  expect(native.orderedRecordingReserve.mock.calls).toHaveLength(2)
  expect(native.orderedRecordingReserve.mock.calls[0]).toEqual(native.orderedRecordingReserve.mock.calls[1])
  expect(JSON.parse(native.orderedRecordingReserve.mock.calls[0][0])[0]).toBe('r')
  expect(native.orderedRecordingStart.mock.calls[0][0]).toBe('native-handle')
  await handle.retire()
  expect(native.orderedRecordingRetire).toHaveBeenCalledWith('native-handle')
})

test('independent handles retire in any order using their own native identities', async () => {
  const native = session()
  native.orderedRecordingReserve.mockImplementation(async reservation => envelope({ version: 2, state: 'reserved', handle: `native-${JSON.parse(reservation)[0]}` }))
  native.orderedRecordingStart.mockImplementation(async (handle, id) => envelope({ ...wireReceipt, receipt: { ...receipt.receipt, recordingId: id } }))
  const first = createReactNativeOrderedRecordingStart(native)
  const second = createReactNativeOrderedRecordingStart(native)
  await first.start(input)
  await second.start({ ...input, id: 'other' })
  await second.retire()
  await first.retire()
  expect(native.orderedRecordingRetire.mock.calls).toEqual([['native-other'], ['native-r']])
})

test('started receipt requires private capability and never returns it publicly', async () => {
  const native = session()
  const retain = jest.fn()
  const handle = createReactNativeOrderedRecordingStart(native, retain)
  const result = await handle.start(input)
  expect(result).toEqual(receipt)
  expect(JSON.stringify(result)).not.toContain('private-capability')
  expect(retain).toHaveBeenCalledWith('18446744073709551615', 'private-capability')
  const malformed = session()
  malformed.orderedRecordingStart.mockResolvedValue(envelope(receipt))
  await expect(createReactNativeOrderedRecordingStart(malformed).start(input)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('rejects an older reservation reply before dispatching start', async () => {
  const native = session()
  native.orderedRecordingReserve.mockResolvedValue(envelope({ version: 1, state: 'reserved', handle: 'old' }))
  await expect(createReactNativeOrderedRecordingStart(native).start(input)).rejects.toThrow()
  expect(native.orderedRecordingStart).not.toHaveBeenCalled()
})

test('independently evaluated copies reserve distinct callers on the same session', async () => {
  let firstFactory, secondFactory
  jest.isolateModules(() => { firstFactory = require('../../../src/backends/reactnative/react-native-ordered-recording-start').createReactNativeOrderedRecordingStart })
  jest.isolateModules(() => { secondFactory = require('../../../src/backends/reactnative/react-native-ordered-recording-start').createReactNativeOrderedRecordingStart })
  const native = session()
  await firstFactory(native).start(input)
  await secondFactory(native).start(input)
  expect(native.orderedRecordingReserve.mock.calls[0][0]).not.toBe(native.orderedRecordingReserve.mock.calls[1][0])
})

test('cached start status observes terminal revocation from another session', async () => {
  const native = session()
  const handle = createReactNativeOrderedRecordingStart(native)
  await handle.start(input)
  native.orderedRecordingStartStatus.mockResolvedValue(envelope({ version: 1, state: 'revoked', receipt: null }))
  await expect(handle.status()).rejects.toMatchObject({ normalized: { code: 'ownership.denied' } })
})

test('retiring a cached start observes revocation before discarding its receipt', async () => {
  const native = session(); const revoked = jest.fn()
  const handle = createReactNativeOrderedRecordingStart(native, undefined, undefined, revoked)
  await handle.start(input)
  native.orderedRecordingStartStatus.mockResolvedValue(envelope({ version: 1, state: 'revoked', receipt: null }))
  await handle.retire()
  expect(revoked).toHaveBeenCalledTimes(1)
  expect(native.orderedRecordingRetire).toHaveBeenCalledTimes(1)
})
