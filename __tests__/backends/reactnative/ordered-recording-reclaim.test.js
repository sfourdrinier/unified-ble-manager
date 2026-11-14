// __tests__/backends/reactnative/ordered-recording-reclaim.test.js
const { createReactNativeRecordingReclaims } = require('../../../src/backends/reactnative/react-native-ordered-recording-reclaim')
const { parseOpValue } = require('../../../src/backends/reactnative/rust-core-wire')

test('forget acknowledgement accepts only exact terminal receipt states', () => {
  const op = 'gatt.ordered.recording-reclaim.ack-forget'
  expect(parseOpValue(op, { state: 'acknowledged' }).ok).toBe(true)
  expect(parseOpValue(op, { state: 'absent' }).ok).toBe(true)
  expect(parseOpValue(op, { state: 'forgotten' }).ok).toBe(false)
  expect(parseOpValue(op, { state: 'acknowledged', extra: true }).ok).toBe(false)
})

test('replays exact reservation after lost reply and publishes only after committed release', async () => {
  const calls = []
  let reserveLost = true
  let commitLost = true
  let releaseCount = 0
  let ackLost = true
  let forgetLost = true
  const failure = { code: 'platform.failure', domain: 'cleanup', operation: 'release', detail: null, platform: null }
  const invoke = jest.fn(async (op, args) => {
    calls.push([op, args])
    if (op.endsWith('.next')) return { requestId: '18446744073709551615' }
    if (op.endsWith('.reserve')) { if (reserveLost) { reserveLost = false; throw new Error('lost reserve') } return { state: 'reserved', mappingCommitted: false, error: null } }
    if (op.endsWith('.start')) return { state: 'ready', mappingCommitted: false, error: null }
    if (op.endsWith('.commit')) { if (commitLost) { commitLost = false; throw new Error('lost commit') } return { state: 'committed', mappingCommitted: true, error: null } }
    if (op.endsWith('.status')) return { state: 'committed', mappingCommitted: true, error: null }
    if (op.endsWith('.release')) return ++releaseCount === 1 ? { state: 'release-failed', mappingCommitted: true, error: failure } : { state: 'released', mappingCommitted: true, error: null }
    if (op.endsWith('.forget-retire')) { if (forgetLost) { forgetLost = false; throw new Error('lost retirement forget') } return { state: 'forgotten' } }
    if (op.endsWith('.ack-forget')) return { state: 'acknowledged' }
    if (op.endsWith('.ack-retire')) { if (ackLost) { ackLost = false; throw new Error('lost retirement ack') } return { state: 'acknowledged' } }
    return { state: 'retired' }
  })
  const publish = jest.fn(async () => {})
  const reserve = createReactNativeRecordingReclaims(invoke, { peerId: 'p', collectionId: '4', recordingId: 'r', reclaimCapability: 'sensitive' }, () => 'stable-key', publish)
  const handle = reserve()
  await expect(handle.start()).rejects.toThrow('lost reserve')
  expect((await handle.start()).state).toBe('ready')
  await expect(handle.commit()).rejects.toThrow('lost commit')
  expect((await handle.status()).state).toBe('committed')
  expect((await handle.release()).state).toBe('release-failed')
  expect(publish).not.toHaveBeenCalled()
  expect((await handle.release()).state).toBe('released')
  expect(publish).toHaveBeenCalledTimes(1)
  const reservations = calls.filter(([op]) => op.endsWith('.reserve')).map(([, args]) => args)
  expect(calls.find(([op]) => op.endsWith('.next'))[1]).toMatchObject({ callerKey: 'stable-key' })
  expect(reservations).toHaveLength(2)
  expect(reservations[0]).toEqual(reservations[1])
  expect(reservations[0]).toMatchObject({ requestId: '18446744073709551615', callerKey: 'stable-key' })
  await expect(handle.retire()).rejects.toThrow('lost retirement ack')
  await expect(handle.retire()).rejects.toThrow('lost retirement forget')
  await handle.retire()
  expect(calls.filter(([op]) => op.endsWith('.retire'))).toHaveLength(1)
  expect(calls.filter(([op]) => op.endsWith('.ack-retire'))).toHaveLength(2)
  expect(calls.filter(([op]) => op.endsWith('.forget-retire'))).toHaveLength(2)
})

test('maps a definite competing reservation refusal without retiring the winner', async () => {
  const failure = { code: 'lifecycle.invalid-state', domain: 'cleanup', operation: 'reclaim.reserve', detail: null, platform: null }
  const invoke = jest.fn(async op => {
    if (op.endsWith('.next')) return { requestId: '7' }
    if (op.endsWith('.reserve')) return { state: 'not-reserved', error: failure }
    throw new Error(`unexpected operation: ${op}`)
  })
  const handle = createReactNativeRecordingReclaims(
    invoke,
    { peerId: 'p', collectionId: '4', recordingId: 'r', reclaimCapability: 'private' },
    () => 'loser',
    async () => {}
  )()
  expect(await handle.start()).toMatchObject({ state: 'not-reserved', error: { code: 'lifecycle.invalid-state' } })
  await handle.retire()
  expect(invoke.mock.calls.map(([op]) => op)).toEqual([
    'gatt.ordered.recording-reclaim.next',
    'gatt.ordered.recording-reclaim.reserve'
  ])
})

test('retirement reconciles a committed release whose reply was lost before forgetting ownership', async () => {
  let publishAttempts = 0
  const calls = []
  const invoke = jest.fn(async op => {
    calls.push(op)
    if (op.endsWith('.next')) return { requestId: '4' }
    if (op.endsWith('.reserve')) return { state: 'reserved', mappingCommitted: false, error: null }
    if (op.endsWith('.start')) return { state: 'ready', mappingCommitted: false, error: null }
    if (op.endsWith('.commit')) return { state: 'committed', mappingCommitted: true, error: null }
    if (op.endsWith('.release')) throw new Error('release response lost')
    if (op.endsWith('.retire')) return { state: 'retired', mappingCommitted: true, error: null }
    if (op.endsWith('.ack-retire')) return { state: 'acknowledged' }
    if (op.endsWith('.forget-retire')) return { state: 'forgotten' }
    if (op.endsWith('.ack-forget')) return { state: 'acknowledged' }
    throw new Error(`unexpected operation: ${op}`)
  })
  const publish = jest.fn(async () => { if (++publishAttempts === 1) throw new Error('activation failed') })
  const handle = createReactNativeRecordingReclaims(invoke, { peerId: 'p', collectionId: '4', recordingId: 'r', reclaimCapability: 'secret' }, () => 'key', publish)()
  await handle.start()
  await handle.commit()
  await expect(handle.release()).rejects.toThrow('release response lost')
  await expect(handle.retire()).rejects.toThrow('activation failed')
  expect(calls).not.toContain('gatt.ordered.recording-reclaim.ack-retire')
  await handle.retire()
  expect(publish).toHaveBeenCalledTimes(2)
  expect(calls.filter(op => op.endsWith('.retire'))).toHaveLength(2)
  expect(calls.at(-1)).toBe('gatt.ordered.recording-reclaim.ack-forget')
})

test('a failed local activation does not hide the exact released native receipt from status', async () => {
  const invoke = jest.fn(async op => {
    if (op.endsWith('.next')) return { requestId: '4' }
    if (op.endsWith('.reserve')) return { state: 'reserved', mappingCommitted: false, error: null }
    if (op.endsWith('.start')) return { state: 'ready', mappingCommitted: false, error: null }
    if (op.endsWith('.release') || op.endsWith('.status')) return { state: 'released', mappingCommitted: true, error: null }
    throw new Error(`unexpected operation: ${op}`)
  })
  const handle = createReactNativeRecordingReclaims(invoke, { peerId: 'p', collectionId: '4', recordingId: 'r', reclaimCapability: 'secret' }, () => 'key', async () => { throw new Error('graph became stale') })()
  await handle.start()
  await expect(handle.release()).rejects.toThrow('graph became stale')
  expect(await handle.status()).toEqual({ state: 'released', mappingCommitted: true, error: null })
})
