// __tests__/backends/reactnative/ordered-recording-public.test.js
const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeManagerHost } = require('../../../src/react-native-manager')
const envelope = value => JSON.stringify({ ok: true, value })
const expectCode = (fn, code) => { let error; try { fn() } catch (caught) { error = caught } expect(error).toMatchObject({ code }) }
const receipt = { kind: 'started', receipt: { version: 1, recordingId: 'capture', collectionId: '4', state: 'active', sourceGeneration: { kind: 'counter', value: '1' }, members: [{ memberId: '0', acquisition: 'acquired', delivery: 'notification', cleanupReleased: false, cleanupError: null }], setupFailure: null, fenceFailure: null, error: null, inspectionError: null, driverError: null } }

// Exercise the legacy transport and JS scheduling with a synthetic gate. It
// does not claim that wire v2 supplies the native authority contract.
beforeEach(() => jest.spyOn(require('../../../src/backends/reactnative/react-native-recording-authority'), 'supportsReactNativeRecordingAuthority').mockImplementation(version => version === 2))
afterEach(() => jest.restoreAllMocks())

async function publicFixture() {
  const harness = rustCoreHarness(); const native = harness.native
  const invoke = native.invoke.bind(native)
  native.invoke = async (session, op, args) => op === 'gatt.ordered.recording-reclaim.version' ? envelope({ wireVersion: 2 }) : invoke(session, op, args)
  native.orderedRecordingVersion = jest.fn(async () => 1)
  native.orderedRecordingReserve = jest.fn(async () => envelope({ version: 2, state: 'reserved', handle: 'native-handle' }))
  native.orderedRecordingStart = jest.fn(async () => envelope({ ...receipt, reclaimCapability: 'private-capability' }))
  native.orderedRecordingStartStatus = jest.fn(async () => envelope({ version: 1, state: 'ready', receipt: { ...receipt, reclaimCapability: 'private-capability' } }))
  native.orderedRecordingRetire = jest.fn(async () => envelope({ version: 1, state: 'retired' }))
  native.orderedRecordingContext = jest.fn()
  native.orderedRecordingStop = jest.fn(async () => envelope({ recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'not-applicable' } }))
  harness.binding = require('../../../src/backends/reactnative/react-native-rust-core-binding').createReactNativeRustCoreBinding({ platform: 'android', native })
  const host = await createReactNativeManagerHost(environment(harness))
  const peer = host.manager.attachedBackend.backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await host.manager.connect(peer, { signal: null, deadline: null })
  const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  const characteristic = database.service('0000180d-0000-1000-8000-00805f9b34fb').characteristic('00002a37-0000-1000-8000-00805f9b34fb')
  const members = [{ memberId: 'a', characteristic }]
  const options = { recording: { id: 'capture', maxBytes: 1048576, maxRecords: 100 } }
  return { native, host, connection, database, members, options, harness }
}

test.each([0, 2, 3])('a refused version %s cannot claim the public recording ID', async version => {
  const { native, host, database, members, options } = await publicFixture()
  try {
    native.orderedRecordingVersion.mockResolvedValueOnce(version)
    const refused = database.reserveOrderedRecordingStart()
    await expect(refused.start(members, options)).rejects.toMatchObject({ code: 'protocol.incompatible' })
    expect(native.orderedRecordingReserve).not.toHaveBeenCalled()
    expect(native.orderedRecordingStart).not.toHaveBeenCalled()
    const good = database.reserveOrderedRecordingStart()
    await expect(good.start(members, options)).resolves.toMatchObject({ kind: 'started' })
    await good.retire()
    await refused.retire()
  } finally { await host.manager.destroy() }
})

test('abort during version negotiation does not claim the recording ID or reserve native state', async () => {
  const { native, host, database, members, options } = await publicFixture()
  try {
    let resolveVersion
    native.orderedRecordingVersion.mockImplementationOnce(() => new Promise(resolve => { resolveVersion = resolve }))
    const abort = new AbortController()
    const refused = database.reserveOrderedRecordingStart()
    const pending = refused.start(members, { ...options, signal: abort.signal })
    while (!resolveVersion) await Promise.resolve()
    abort.abort(); resolveVersion(1)
    await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
    expect(native.orderedRecordingReserve).not.toHaveBeenCalled()
    const good = database.reserveOrderedRecordingStart()
    await expect(good.start(members, options)).resolves.toMatchObject({ kind: 'started' })
    await good.retire(); await refused.retire()
  } finally { await host.manager.destroy() }
})

test.each([0, 2, 3])('public stop refuses receipt offer %s before native stop', async version => {
  const { native, host } = await publicFixture()
  try {
    native.orderedRecordingVersion.mockResolvedValueOnce(version)
    await expect(host.services.orderedRecordingStop('capture')).rejects.toMatchObject({ code: 'protocol.incompatible' })
    expect(native.orderedRecordingStop).not.toHaveBeenCalled()
  } finally { await host.manager.destroy() }
})

test('public stop preserves uncertain cleanup and rejects a skewed post-dispatch receipt', async () => {
  const { native, host } = await publicFixture()
  try {
    native.orderedRecordingStop.mockResolvedValueOnce(envelope({ recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'uncertain' } }))
    expect(await host.services.orderedRecordingStop('capture')).toEqual({ recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'uncertain' } })
    native.orderedRecordingStop.mockResolvedValueOnce(envelope({ recording: { version: 2, state: 'finished', error: null }, hostCleanup: { state: 'uncertain' } }))
    await expect(host.services.orderedRecordingStop('capture')).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(native.orderedRecordingStop).toHaveBeenCalledTimes(2)
  } finally { await host.manager.destroy() }
})
