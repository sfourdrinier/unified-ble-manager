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
  native.invoke = async (session, op, args) => op === 'gatt.ordered.recording-reclaim.version' ? envelope({ wireVersion: 2 }) : op === 'gatt.ordered.marker.version' ? envelope({ wireVersion: 1 }) : invoke(session, op, args)
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

test('marker reservation before the React Native recording is active throws a public BleError synchronously', async () => {
  const { host, database } = await publicFixture()
  try {
    const start = database.reserveOrderedRecordingStart()
    const { BleError } = require('../../../src/public/errors')
    let failure
    try { start.reserveMarkerWrite() } catch (error) { failure = error }
    expect(failure).toBeInstanceOf(BleError)
    expect(failure.code).toBe('lifecycle.invalid-state')
    await start.retire()
  } finally { await host.manager.destroy() }
})

test('version 2 alone cannot grant cross-session reclaim authority after start receipt retirement', async () => {
  require('../../../src/backends/reactnative/react-native-recording-authority').supportsReactNativeRecordingAuthority.mockReturnValue(false)
  const { host, database, members, options, harness } = await publicFixture()
  const start = database.reserveOrderedRecordingStart()
  await start.start(members, options)
  await start.retire()
  const other = await createReactNativeManagerHost(environment(harness, { clientId: 'other-client', managerId: 'other-manager', hostSessionScope: 'other-scope' }))
  await other.services.orderedRecordingStop('capture')
  expect(start.recoveryHandle).toBeUndefined()
  expect(host.services.reserveOrderedRecordingReclaim).toBeUndefined()
  expect(() => host.services.orderedRecordingRecoveryHandle('capture')).toThrow('capability.unsupported')
  expect(() => database.reserveOrderedRecordingReclaim({ kind: 'ordered-recording-recovery' })).toThrow('capability.unsupported')
  await other.manager.destroy()
  await host.manager.destroy()
})

test('invalid bounds cannot seize an id from a valid public start', async () => {
  const { host, database, members, options } = await publicFixture()
  const bad = database.reserveOrderedRecordingStart(); const good = database.reserveOrderedRecordingStart()
  await expect(bad.start(members, { recording: { ...options.recording, maxBytes: 0 } })).rejects.toThrow()
  await good.start(members, options)
  expect(good.recoveryHandle()).toEqual({ kind: 'ordered-recording-recovery' })
  await bad.retire(); await good.retire(); await host.manager.destroy()
})

test.each(['failed', 'retired'])('public start reports a native %s status', async state => {
  const { native, host, database, members, options } = await publicFixture()
  native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  native.orderedRecordingStartStatus.mockResolvedValue(envelope({ version: 1, state, receipt: null }))
  const handle = database.reserveOrderedRecordingStart()
  await expect(handle.start(members, options)).rejects.toThrow()
  expect(await handle.status()).toEqual({ state })
  await handle.retire(); await host.manager.destroy()
})

test.each(['owner-first', 'contender-first'])('independent starts cannot share a lost native receipt (%s)', async order => {
  const { native, host, database, members, options } = await publicFixture()
  native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  const owner = database.reserveOrderedRecordingStart(); const contender = database.reserveOrderedRecordingStart()
  const first = owner.start(members, options)
  const second = contender.start(members, options)
  const outcomes = await Promise.allSettled([first, second])
  expect(outcomes[0].status).toBe('rejected')
  expect(outcomes[1]).toMatchObject({ status: 'rejected', reason: { code: 'ownership.denied' } })
  if (order === 'contender-first') await contender.retire()
  await owner.retire()
  expect(owner.recoveryHandle()).toEqual({ kind: 'ordered-recording-recovery' })
  if (order === 'owner-first') await contender.retire()
  expect(native.orderedRecordingReserve).toHaveBeenCalledTimes(1)
  expect(native.orderedRecordingRetire).toHaveBeenCalledTimes(1)
  await host.manager.destroy()
})

test('late start receipt cannot restore authority after terminal stop', async () => {
  const { native, host, database, members, options } = await publicFixture()
  let resolveStart
  native.orderedRecordingStart.mockImplementation(() => new Promise(resolve => { resolveStart = resolve }))
  const handle = database.reserveOrderedRecordingStart()
  const pending = handle.start(members, options)
  while (!resolveStart) await Promise.resolve()
  await host.services.orderedRecordingStop('capture')
  resolveStart(envelope({ ...receipt, reclaimCapability: 'private-capability' }))
  await pending
  expectCode(() => handle.recoveryHandle(), 'ownership.denied')
  expect(() => host.services.orderedRecordingRecoveryHandle('capture')).toThrow('ownership.denied')
  expect(() => handle.reserveMarkerWrite()).toThrow()
  await handle.retire(); await host.manager.destroy()
})

test('observing a cross-session revocation removes cached public authorities', async () => {
  const { native, host, database, members, options } = await publicFixture()
  const handle = database.reserveOrderedRecordingStart()
  await handle.start(members, options)
  const recovery = handle.recoveryHandle()
  native.orderedRecordingStartStatus.mockResolvedValue(envelope({ version: 1, state: 'revoked', receipt: null }))
  await expect(handle.status()).rejects.toMatchObject({ code: 'ownership.denied' })
  expectCode(() => handle.recoveryHandle(), 'ownership.denied')
  expect(() => handle.reserveMarkerWrite()).toThrow()
  expect(() => database.reserveOrderedRecordingReclaim(recovery)).toThrow()
  await handle.retire(); await host.manager.destroy()
})

test('public reclaim refuses a different database before publishing native ownership', async () => {
  const { native, host, connection, database, members, options } = await publicFixture()
  const start = database.reserveOrderedRecordingStart()
  await start.start(members, options)
  const handoff = start.recoveryHandle()
  native.changeDatabase(DEFAULT_PEER)
  const replacement = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  const invoke = native.invoke.bind(native); const calls = []
  native.invoke = async (session, op, args) => {
    if (!op.startsWith('gatt.ordered.recording-reclaim.')) return invoke(session, op, args)
    calls.push(op)
    if (op.endsWith('.next')) return envelope({ requestId: '0' })
    if (op.endsWith('.ack-retire') || op.endsWith('.ack-forget')) return envelope({ state: 'acknowledged' })
    if (op.endsWith('.forget-retire')) return envelope({ state: 'forgotten' })
    if (op.endsWith('.retire')) return envelope({ state: 'retired', mappingCommitted: false, error: null })
    return envelope({ state: op.endsWith('.reserve') ? 'reserved' : 'ready', mappingCommitted: false, error: null })
  }
  const reclaim = replacement.reserveOrderedRecordingReclaim(handoff)
  expect((await reclaim.start()).state).toBe('ready')
  await expect(reclaim.commit()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
  expect(calls).not.toContain('gatt.ordered.recording-reclaim.commit')
  await reclaim.retire(); await start.retire(); await host.manager.destroy()
})

test('terminal stop fences a late reclaim receipt while preserving retirement', async () => {
  const { native, host, database, members, options } = await publicFixture()
  const start = database.reserveOrderedRecordingStart(); await start.start(members, options)
  const handoff = start.recoveryHandle(); let resolveRelease
  const invoke = native.invoke.bind(native)
  native.invoke = async (session, op, args) => {
    if (!op.startsWith('gatt.ordered.recording-reclaim.')) return invoke(session, op, args)
    if (op.endsWith('.next')) return envelope({ requestId: '0' })
    if (op.endsWith('.ack-retire') || op.endsWith('.ack-forget')) return envelope({ state: 'acknowledged' })
    if (op.endsWith('.forget-retire')) return envelope({ state: 'forgotten' })
    if (op.endsWith('.release')) return new Promise(resolve => { resolveRelease = resolve })
    if (op.endsWith('.retire')) return envelope({ state: 'retired', mappingCommitted: true, error: null })
    const state = op.endsWith('.reserve') ? 'reserved' : op.endsWith('.commit') ? 'committed' : 'ready'
    return envelope({ state, mappingCommitted: state === 'committed', error: null })
  }
  const reclaim = database.reserveOrderedRecordingReclaim(handoff)
  expect((await reclaim.start()).state).toBe('ready')
  expect((await reclaim.commit()).state).toBe('committed')
  expect(native.orderedRecordingContext).not.toHaveBeenCalled()
  const releasing = reclaim.release()
  while (!resolveRelease) await Promise.resolve()
  await host.services.orderedRecordingStop('capture')
  resolveRelease(envelope({ state: 'released', mappingCommitted: true, error: null }))
  expect((await releasing).state).toBe('released')
  await reclaim.retire()
  expect(() => host.services.orderedRecordingRecoveryHandle('capture')).toThrow('ownership.denied')
  await start.retire(); await host.manager.destroy()
})

test('lost committed reclaim response is reconciled even if the target later becomes stale', async () => {
  const { native, host, connection, database, members, options } = await publicFixture()
  const start = database.reserveOrderedRecordingStart(); await start.start(members, options)
  const handoff = start.recoveryHandle(); let committed = false; let commits = 0
  const invoke = native.invoke.bind(native)
  native.invoke = async (session, op, args) => {
    if (!op.startsWith('gatt.ordered.recording-reclaim.')) return invoke(session, op, args)
    if (op.endsWith('.next')) return envelope({ requestId: '0' })
    if (op.endsWith('.ack-retire') || op.endsWith('.ack-forget')) return envelope({ state: 'acknowledged' })
    if (op.endsWith('.forget-retire')) return envelope({ state: 'forgotten' })
    if (op.endsWith('.retire')) return envelope({ state: 'retired', mappingCommitted: false, error: null })
    if (op.endsWith('.commit')) { commits++; committed = true; throw new Error('lost commit reply') }
    const state = op.endsWith('.reserve') ? 'reserved' : committed ? 'committed' : 'ready'
    return envelope({ state, mappingCommitted: state === 'committed', error: null })
  }
  const reclaim = database.reserveOrderedRecordingReclaim(handoff)
  expect((await reclaim.start()).state).toBe('ready')
  await expect(reclaim.commit()).rejects.toThrow()
  native.changeDatabase(DEFAULT_PEER)
  await connection.discover({ signal: null, deadline: null })
  expect(await reclaim.commit()).toMatchObject({ state: 'committed', mappingCommitted: true })
  expect(commits).toBe(1)
  await reclaim.retire(); await start.retire(); await host.manager.destroy()
})

test('rejects the older reclaim wire before exposing a session', async () => {
  const harness = rustCoreHarness(); const native = harness.native
  native.orderedRecordingVersion = jest.fn(async () => 1)
  for (const name of ['orderedRecordingReserve', 'orderedRecordingStartStatus', 'orderedRecordingRetire', 'orderedRecordingStart', 'orderedRecordingContext', 'orderedRecordingStop']) native[name] = jest.fn()
  const invoke = native.invoke.bind(native)
  native.invoke = async (session, op, args) => op === 'gatt.ordered.recording-reclaim.version'
    ? envelope({ wireVersion: 1 }) : invoke(session, op, args)
  harness.binding = require('../../../src/backends/reactnative/react-native-rust-core-binding').createReactNativeRustCoreBinding({ platform: 'android', native })
  await expect(createReactNativeManagerHost(environment(harness))).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
})

test('public ordered recording start recovers lost reply, session stop and private loss replay', async () => {
  const harness = rustCoreHarness(); const native = harness.native
  const invoke = native.invoke.bind(native)
  const markerCalls = []
  let detached = false
  let publicRetry = false
  let reserveLost = false
  let releaseFailed = false
  let ackLost = false
  const reclaimCalls = []
  native.invoke = async (session, op, args) => {
    if (op === 'gatt.ordered.recording-reclaim.version') return envelope({ wireVersion: 2 })
    if (op.startsWith('gatt.ordered.recording-reclaim.')) {
      const input = JSON.parse(args)
      reclaimCalls.push([op, input])
      if ((!detached || input.peerId !== DEFAULT_PEER || input.recordingId !== 'capture' || input.reclaimCapability !== 'private-capability') && op.endsWith('.next'))
        return envelope({ state: 'not-reserved', error: { code: 'ownership.denied', domain: 'gatt', operation: op, detail: null, platform: null } })
      if (!detached || input.peerId !== DEFAULT_PEER || input.recordingId !== 'capture' || input.reclaimCapability !== 'private-capability')
        return JSON.stringify({ ok: false, error: { code: 'ownership.denied', domain: 'gatt', operation: op, detail: null }, commit: null, retryability: 'never' })
      if (op.endsWith('.next')) return envelope({ requestId: '0' })
      if (publicRetry && op.endsWith('.reserve') && !reserveLost) { reserveLost = true; throw new Error('lost reserve') }
      if (publicRetry && op.endsWith('.release') && !releaseFailed) {
        releaseFailed = true
        return envelope({ state: 'release-failed', mappingCommitted: true, error: { code: 'platform.failure', domain: 'cleanup', operation: op, detail: null, platform: null } })
      }
      if (publicRetry && op.endsWith('.ack-retire') && !ackLost) { ackLost = true; throw new Error('lost ack') }
      if (op.endsWith('.forget-retire')) return envelope({ state: 'forgotten' })
      if (op.endsWith('.ack-forget')) return envelope({ state: 'acknowledged' })
      if (op.endsWith('.ack-retire')) return envelope({ state: 'acknowledged' })
      if (op.endsWith('.retire')) return envelope({ state: 'retired', mappingCommitted: true, error: null })
      const state = op.endsWith('.reserve') ? 'reserved' : op.endsWith('.start') || op.endsWith('.status') ? 'ready' : op.endsWith('.commit') ? 'committed' : 'released'
      return envelope({ state, mappingCommitted: state === 'committed' || state === 'released', error: null })
    }
    if (!op.startsWith('gatt.ordered.marker.')) return invoke(session, op, args)
    markerCalls.push([op, JSON.parse(args)])
    if (op === 'gatt.ordered.marker.version') return envelope({ wireVersion: 1 })
    if (op === 'gatt.ordered.marker.reserve') return envelope({ handle: '8' })
    if (op === 'gatt.ordered.marker.start') return envelope({ state: 'admitted', ordinal: '9', write: { state: 'settled', error: null } })
    return envelope(null)
  }
  native.orderedRecordingVersion = jest.fn(async () => 1)
  native.orderedRecordingReserve = jest.fn(async () => envelope({ version: 2, state: 'reserved', handle: 'native-handle' }))
  let recordedContext
  native.orderedRecordingStart = jest.fn(async (...args) => { recordedContext = JSON.parse(args[5]); throw new Error('lost response') })
  native.orderedRecordingStartStatus = jest.fn(async () => envelope({ version: 1, state: 'ready', receipt: { ...receipt, reclaimCapability: 'private-capability' } }))
  native.orderedRecordingRetire = jest.fn(async () => envelope({ version: 1, state: 'retired' }))
  native.orderedRecordingContext = jest.fn(async () => envelope(recordedContext))
  native.orderedRecordingStop = jest.fn(async () => envelope({ recording: { version: 1, state: 'waiting-drain', error: null }, hostCleanup: { state: 'not-applicable' } }))
  const rows = [{ ordinal: 1, metadata: { kind: 'ordered', version: 1, recordingId: 'capture', peerId: DEFAULT_PEER, collectionId: '4', sourceGeneration: { kind: 'counter', value: '1' }, deliveryId: '1' }, record: { kind: 'loss', scope: { kind: 'unknown' }, unit: 'source-records', count: null, bytes: null } }]
  native.continuationRecordingPrepare = jest.fn(async () => envelope({ token: 'capture:1', records: rows, bytes: Buffer.byteLength(JSON.stringify(rows[0])), more: false }))
  harness.binding = require('../../../src/backends/reactnative/react-native-rust-core-binding').createReactNativeRustCoreBinding({ platform: 'android', native })
  const host = await createReactNativeManagerHost(environment(harness))
  const otherPeer = 'A0:9E:1A:00:00:02'
  native.peripherals.set(otherPeer, { ...native.peripherals.get(DEFAULT_PEER), peerId: otherPeer })
  native.initialBonds.set(otherPeer, false)
  const backend = host.manager.attachedBackend.backend
  const peerId = backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const connection = await host.manager.connect(peerId, { signal: null, deadline: null })
  const database = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  const characteristic = database.service('0000180d-0000-1000-8000-00805f9b34fb').characteristic('00002a37-0000-1000-8000-00805f9b34fb')
  const handle = database.reserveOrderedRecordingStart()
  expect(() => handle.recoveryHandle()).toThrow()
  await expect(handle.start([{ memberId: 'a', characteristic }], { recording: { id: 'capture', maxBytes: 1048576, maxRecords: 100 } })).rejects.toThrow()
  const publicStatus = await handle.status()
  expect(publicStatus).toEqual({ state: 'result', result: receipt })
  expect(JSON.stringify(publicStatus)).not.toContain('private-capability')
  const publicRecovery = handle.recoveryHandle()
  expect(publicRecovery).toEqual({ kind: 'ordered-recording-recovery' })
  expect(handle.recoveryHandle()).toBe(publicRecovery)
  expect(JSON.stringify(publicRecovery)).not.toContain('private-capability')
  expect(() => database.reserveOrderedRecordingReclaim({ ...publicRecovery })).toThrow()
  const otherPeerId = backend.connections.peerFromAddress({ address: otherPeer, addressType: 'public' })
  const otherConnection = await host.manager.connect(otherPeerId, { signal: null, deadline: null })
  const otherDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await otherConnection.discover({ signal: null, deadline: null }))
  expectCode(() => otherDatabase.reserveOrderedRecordingReclaim(publicRecovery), 'ownership.denied')
  const foreignHost = await createReactNativeManagerHost(environment(harness))
  const foreignBackend = foreignHost.manager.attachedBackend.backend
  const foreignPeer = foreignBackend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' })
  const foreignConnection = await foreignHost.manager.connect(foreignPeer, { signal: null, deadline: null })
  const foreignDatabase = await require('../../../src/public/gatt').createPublicGattDatabase(await foreignConnection.discover({ signal: null, deadline: null }))
  expectCode(() => foreignDatabase.reserveOrderedRecordingReclaim(publicRecovery), 'ownership.denied')
  await foreignHost.manager.destroy()
  const publicDenied = database.reserveOrderedRecordingReclaim(publicRecovery)
  expect(await publicDenied.start()).toMatchObject({ state: 'not-reserved', error: { code: 'ownership.denied' } })
  await publicDenied.retire()
  const recovery = host.services.orderedRecordingRecoveryHandle('capture')
  expect(recovery).toEqual({ recordingId: 'capture', peerId: DEFAULT_PEER, collectionId: '4', reclaimCapability: 'private-capability' })
  const denied = host.services.reserveOrderedRecordingReclaim(recovery)
  await expect(denied.start()).resolves.toMatchObject({ state: 'not-reserved', error: { code: 'ownership.denied' } })
  await denied.retire()
  detached = true
  const wrong = host.services.reserveOrderedRecordingReclaim({ ...recovery, recordingId: 'other' })
  await expect(wrong.start()).resolves.toMatchObject({ state: 'not-reserved', error: { code: 'ownership.denied' } })
  await wrong.retire()
  const mutableHandoff = { ...recovery }
  const reclaimed = host.services.reserveOrderedRecordingReclaim(mutableHandoff)
  mutableHandoff.recordingId = 'mutated'
  mutableHandoff.reclaimCapability = 'mutated'
  expect((await reclaimed.start()).state).toBe('ready')
  expect((await reclaimed.commit()).state).toBe('committed')
  expect((await reclaimed.release()).state).toBe('released')
  expect((await reclaimed.context()).peerId).toBe(DEFAULT_PEER)
  expect(reclaimed.stream).toBeUndefined()
  expect(JSON.stringify(reclaimed)).not.toContain('private-capability')
  await reclaimed.retire()
  const newer = host.services.reserveOrderedRecordingReclaim(recovery)
  await reclaimed.retire()
  expect(() => host.services.reserveOrderedRecordingReclaim(recovery)).toThrow()
  await newer.retire()
  publicRetry = true
  const publicReclaim = database.reserveOrderedRecordingReclaim(publicRecovery)
  expect(() => database.reserveOrderedRecordingReclaim(publicRecovery)).toThrow()
  await expect(publicReclaim.start()).rejects.toThrow()
  expect(await publicReclaim.start()).toMatchObject({ state: 'ready' })
  expect(await publicReclaim.commit()).toMatchObject({ state: 'committed' })
  expect(await publicReclaim.release()).toMatchObject({ state: 'release-failed' })
  expect(await publicReclaim.release()).toMatchObject({ state: 'released' })
  await expect(publicReclaim.retire()).rejects.toThrow()
  await publicReclaim.retire()
  await publicReclaim.retire()
  const reserveCalls = reclaimCalls.filter(([op]) => op.endsWith('.reserve')).slice(-2)
  expect(reserveCalls).toHaveLength(2)
  expect(reserveCalls[0]).toEqual(reserveCalls[1])
  expect(JSON.stringify(publicReclaim)).not.toContain('private-capability')
  const recoveredMarker = database.reserveOrderedRecordingMarker('capture')
  expect(await recoveredMarker.start(characteristic, Uint8Array.from([2]), 'with-response')).toMatchObject({ state: 'admitted' })
  await recoveredMarker.forget()
  const marker = handle.reserveMarkerWrite()
  expect(await marker.start(characteristic, Uint8Array.from([3]), 'with-response')).toMatchObject({ state: 'admitted', ordinal: '9' })
  expect(markerCalls.filter(([op]) => op === 'gatt.ordered.marker.start').at(-1)[1]).toMatchObject({ peerId: DEFAULT_PEER, collectionId: '4', handle: '8', valueB64: 'Aw==' })
  await marker.forget()
  const args = native.orderedRecordingStart.mock.calls[0]
  const declaration = JSON.parse(args[5])
  expect(declaration).toMatchObject({ kind: 'ordered-recording', version: 1, peerId: DEFAULT_PEER, members: [{ memberId: '0', publicMemberId: 'a' }] })
  expect(await host.services.orderedRecordingStop('capture')).toEqual({ recording: { version: 1, state: 'waiting-drain', error: null }, hostCleanup: { state: 'not-applicable' } })
  expect((await host.recordings.prepareOrdered('capture', { maxItems: 10, maxBytes: 4096 })).records[0].record).toMatchObject({ kind: 'loss', scope: { kind: 'unknown' }, count: null })
  const competingStart = database.reserveOrderedRecordingStart()
  await expect(competingStart.start([{ memberId: 'a', characteristic }], { recording: { id: 'capture', maxBytes: 1048576, maxRecords: 100 } })).rejects.toThrow()
  await competingStart.status()
  native.orderedRecordingStop.mockResolvedValueOnce(envelope({ recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'not-applicable' } }))
  await host.services.orderedRecordingStop('capture')
  expect(() => host.services.orderedRecordingRecoveryHandle('capture')).toThrow('ownership.denied')
  await handle.status()
  await competingStart.status()
  expectCode(() => handle.recoveryHandle(), 'ownership.denied')
  expectCode(() => competingStart.recoveryHandle(), 'ownership.denied')
  expectCode(() => database.reserveOrderedRecordingReclaim(publicRecovery), 'ownership.denied')
  await handle.retire()
  await competingStart.retire()
  expect(native.orderedRecordingRetire).toHaveBeenCalledWith(args[0], 'native-handle')
  const replacement = await require('../../../src/public/gatt').createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  expect(() => database.reserveOrderedRecordingReclaim(publicRecovery)).toThrow()
  expect(() => replacement.reserveOrderedRecordingReclaim({ ...publicRecovery })).toThrow()
  await host.manager.destroy()
})

test('a new public start after terminal stop cannot recover the previous receipt', async () => {
  const { native, host, database, members, options } = await publicFixture()
  const reservations = new Map()
  native.orderedRecordingReserve.mockImplementation(async (_session, key) => {
    if (!reservations.has(key)) reservations.set(key, `native-${reservations.size}`)
    return envelope({ version: 2, state: 'reserved', handle: reservations.get(key) })
  })
  native.orderedRecordingStart.mockImplementation(async (_session, handle) => {
    if (handle !== 'native-0') return envelope({ kind: 'prepare-failed', receipt: { version: 1, state: 'prepare-failed', recordingId: 'capture', sourceStarted: false, journalEffect: 'none', error: { kind: 'invalid-identity' } } })
    return envelope({ ...receipt, reclaimCapability: 'private-capability' })
  })
  const first = database.reserveOrderedRecordingStart()
  await first.start(members, options)
  await host.services.orderedRecordingStop('capture')
  const second = database.reserveOrderedRecordingStart()
  expect(await second.start(members, options)).toMatchObject({ kind: 'prepare-failed' })
  expect(reservations.size).toBe(2)
  expectCode(() => second.recoveryHandle(), 'ownership.denied')
  await second.retire()
  expect(native.orderedRecordingRetire.mock.calls.at(-1)[1]).toBe('native-1')
  await first.retire(); await host.manager.destroy()
})

test('retirement does not erase a start receipt observed pending before it becomes ready', async () => {
  const { native, host, database, members, options } = await publicFixture()
  native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  native.orderedRecordingStartStatus.mockResolvedValueOnce(envelope({ version: 1, state: 'pending', receipt: null }))
  const handle = database.reserveOrderedRecordingStart()
  await expect(handle.start(members, options)).rejects.toThrow()
  await expect(handle.retire()).rejects.toThrow()
  expect(native.orderedRecordingRetire).not.toHaveBeenCalled()
  await handle.retire()
  expect(handle.recoveryHandle()).toEqual({ kind: 'ordered-recording-recovery' })
  await host.manager.destroy()
})

test('an in-flight discovery completes while a reclaim commit reply is hung', async () => {
  const { native, host, connection, database, members, options } = await publicFixture()
  const start = database.reserveOrderedRecordingStart(); await start.start(members, options)
  const handoff = start.recoveryHandle()
  const invoke = native.invoke.bind(native)
  let deliverDiscovery; let finishCommit
  native.invoke = async (session, op, args) => {
    if (op === 'gatt.discover') {
      const reply = await invoke(session, op, args)
      return new Promise(resolve => { deliverDiscovery = () => resolve(reply) })
    }
    if (!op.startsWith('gatt.ordered.recording-reclaim.')) return invoke(session, op, args)
    if (op.endsWith('.next')) return envelope({ requestId: '0' })
    if (op.endsWith('.ack-retire') || op.endsWith('.ack-forget')) return envelope({ state: 'acknowledged' })
    if (op.endsWith('.forget-retire')) return envelope({ state: 'forgotten' })
    if (op.endsWith('.retire')) return envelope({ state: 'retired', mappingCommitted: false, error: null })
    if (op.endsWith('.commit')) return new Promise(resolve => { finishCommit = () => resolve(envelope({ state: 'committed', mappingCommitted: true, error: null })) })
    return envelope({ state: op.endsWith('.reserve') ? 'reserved' : 'ready', mappingCommitted: false, error: null })
  }
  const discovery = connection.discover({ signal: null, deadline: null })
  while (!deliverDiscovery) await Promise.resolve()
  const reclaim = database.reserveOrderedRecordingReclaim(handoff)
  await reclaim.start()
  const commit = reclaim.commit()
  while (!finishCommit) await Promise.resolve()
  let discovered = false
  discovery.then(() => { discovered = true })
  deliverDiscovery()
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(discovered).toBe(true)
  finishCommit()
  expect((await commit).state).toBe('committed')
  await discovery
  await reclaim.retire(); await start.retire(); await host.manager.destroy()
})

test.each(['service-change', 'link-loss'])('%s during commit preserves the late committed receipt', async transition => {
  const { native, host, database, members, options } = await publicFixture()
  const start = database.reserveOrderedRecordingStart(); await start.start(members, options)
  const handoff = start.recoveryHandle()
  const invoke = native.invoke.bind(native)
  let finishCommit
  native.invoke = async (session, op, args) => {
    if (!op.startsWith('gatt.ordered.recording-reclaim.')) return invoke(session, op, args)
    if (op.endsWith('.next')) return envelope({ requestId: '0' })
    if (op.endsWith('.commit')) return new Promise(resolve => { finishCommit = () => resolve(envelope({ state: 'committed', mappingCommitted: true, error: null })) })
    if (op.endsWith('.retire')) return envelope({ state: 'retired', mappingCommitted: false, error: null })
    if (op.endsWith('.ack-retire') || op.endsWith('.ack-forget')) return envelope({ state: 'acknowledged' })
    if (op.endsWith('.forget-retire')) return envelope({ state: 'forgotten' })
    return envelope({ state: op.endsWith('.reserve') ? 'reserved' : 'ready', mappingCommitted: false, error: null })
  }
  const reclaim = database.reserveOrderedRecordingReclaim(handoff)
  await reclaim.start()
  const commit = reclaim.commit()
  while (!finishCommit) await Promise.resolve()
  if (transition === 'service-change') native.changeDatabase(DEFAULT_PEER)
  else native.dropLink(DEFAULT_PEER)
  await require('../../../test-support/react-native/rust-core-harness').settle(100)
  finishCommit()
  expect(await commit).toEqual({ state: 'committed', mappingCommitted: true, error: null })
  await reclaim.retire(); await start.retire(); await host.manager.destroy()
})

test.each(['service-change', 'link-loss'])('a delayed discovery cannot restore a graph after %s', async transition => {
  const { native, host, connection } = await publicFixture()
  const invoke = native.invoke.bind(native)
  let deliver
  native.invoke = async (session, op, args) => {
    if (op !== 'gatt.discover') return invoke(session, op, args)
    const reply = await invoke(session, op, args)
    return new Promise(resolve => { deliver = () => resolve(reply) })
  }
  const pending = connection.discover({ signal: null, deadline: null })
  while (!deliver) await Promise.resolve()
  if (transition === 'service-change') native.changeDatabase(DEFAULT_PEER)
  else native.dropLink(DEFAULT_PEER)
  await require('../../../test-support/react-native/rust-core-harness').settle(100)
  deliver()
  await expect(pending).rejects.toThrow(transition === 'service-change' ? 'gatt.stale-handle' : 'connection.stale')
  await host.manager.destroy()
})

test('a graph invalidated while discovery refreshes counters is not published', async () => {
  const { native, host, connection } = await publicFixture()
  const invoke = native.invoke.bind(native)
  let deliverCounters
  native.invoke = async (session, op, args) => {
    const reply = await invoke(session, op, args)
    if (op === 'counters.describe' && !deliverCounters) return new Promise(resolve => { deliverCounters = () => resolve(reply) })
    return reply
  }
  const pending = connection.discover({ signal: null, deadline: null })
  while (!deliverCounters) await Promise.resolve()
  native.changeDatabase(DEFAULT_PEER)
  await require('../../../test-support/react-native/rust-core-harness').settle(100)
  deliverCounters()
  await expect(pending).rejects.toThrow('gatt.stale-handle')
  await host.manager.destroy()
})

test('a terminally revoked lost start can retire without recovering its former authority', async () => {
  const { native, host, database, members, options } = await publicFixture()
  native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  const handle = database.reserveOrderedRecordingStart()
  await expect(handle.start(members, options)).rejects.toThrow()
  await host.services.orderedRecordingStop('capture')
  native.orderedRecordingStartStatus.mockResolvedValue(envelope({ version: 1, state: 'revoked', receipt: null }))
  await handle.retire()
  expectCode(() => handle.recoveryHandle(), 'ownership.denied')
  expect(native.orderedRecordingRetire).toHaveBeenCalledTimes(1)
  await host.manager.destroy()
})

test('a refused pending retirement leaves a lost start retryable on the same reservation', async () => {
  const { native, host, database, members, options } = await publicFixture()
  native.orderedRecordingStart.mockRejectedValueOnce(new Error('lost dispatch'))
  native.orderedRecordingStartStatus.mockResolvedValueOnce(envelope({ version: 1, state: 'reserved', receipt: null }))
  const handle = database.reserveOrderedRecordingStart()
  await expect(handle.start(members, options)).rejects.toThrow()
  await expect(handle.retire()).rejects.toThrow()
  expect((await handle.start(members, options)).kind).toBe('started')
  expect(native.orderedRecordingReserve).toHaveBeenCalledTimes(1)
  await handle.retire(); await host.manager.destroy()
})

test('concurrent retirement attempts share the same pending reconciliation', async () => {
  const { native, host, database, members, options } = await publicFixture()
  native.orderedRecordingStart.mockRejectedValue(new Error('lost reply'))
  let resolveStatus
  native.orderedRecordingStartStatus.mockImplementationOnce(() => new Promise(resolve => { resolveStatus = resolve }))
  const handle = database.reserveOrderedRecordingStart()
  await expect(handle.start(members, options)).rejects.toThrow()
  const first = handle.retire(); const second = handle.retire()
  while (!resolveStatus) await Promise.resolve()
  resolveStatus(envelope({ version: 1, state: 'pending', receipt: null }))
  const outcomes = await Promise.allSettled([first, second])
  expect(outcomes.map(outcome => outcome.status)).toEqual(['rejected', 'rejected'])
  expect(native.orderedRecordingRetire).not.toHaveBeenCalled()
  await handle.retire(); await host.manager.destroy()
})
