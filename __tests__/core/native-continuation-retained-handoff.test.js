const { createNativeContinuationControl } = require('../../src/core/native-continuation-control')

const selector = { serviceUuid: '180d', serviceOccurrence: 1, characteristicUuid: '2a37', characteristicOccurrence: 1 }
const ok = value => JSON.stringify({ ok: true, value })
const prepared = (token, records) => ({
  claimToken: token,
  consumerCount: 1,
  selectors: [selector],
  batches: [JSON.stringify({ records, more: true, controlLost: 0 })],
  disposed: false,
  afterCutoffLoss: { items: 0, bytes: 0 },
  disposeFailure: null
})
const value = ordinal => ({
  t: 'value',
  ordinal,
  consumer: 'ubm-continuation-0',
  valueB64: 'AQ==',
  delivery: 'notification'
})
const receipt = disposed => ({
  disposed,
  afterCutoffLoss: { items: 0, bytes: 0 },
  disposeFailure: disposed ? null : 'retained tail'
})

// `claim()` deliberately retains its 5.2.1 contract: it decodes and ACKs the
// complete legacy handoff.  These retained-handoff scenarios exercise the
// additive, versioned V1 entrypoint instead.
const retainedControl = (prepare, acknowledge, retire = async () => ok({})) =>
  createNativeContinuationControl({
    prepareClaim: prepare,
    acknowledgeClaim: acknowledge,
    prepareRetainedClaimV1: prepare,
    acknowledgeRetainedClaimV1: acknowledge,
    retireRetainedClaimReceiptV1: retire
  })

const recovery = (encodedLength = '3', chunkBytes = 65536) => ({
  t: 'peer-source-failure-recovery',
  v: 2,
  consumer: 'ubm-continuation-0',
  peerId: 'peer-a',
  selector: {
    serviceUuid: '180d',
    serviceOccurrence: 1,
    characteristicUuid: '2a37',
    characteristicOccurrence: 1,
    descriptorUuid: null,
    descriptorOccurrence: null
  },
  recipientLease: '0123456789abcdef0123456789abcdef',
  descriptor: {
    protocol: 1,
    representation: 'ubm-peer-source-failure/1',
    host: '01'.repeat(16),
    peer: '02'.repeat(16),
    sourceGenerationIdentity: '03'.repeat(16),
    nativeGroup: '04'.repeat(16),
    transfer: '05'.repeat(16),
    recovery: '06'.repeat(16),
    encodedLength,
    chunkBytes
  }
})

test('discloses each retained prefix before its explicit acknowledgement, including cleanup tail', async () => {
  const queue = [
    prepared('one', [value(1)]),
    prepared('two', [value(2)]),
    prepared('tail', [
      {
        t: 'stream-end',
        ordinal: 3,
        consumer: 'ubm-continuation-0',
        reason: 'closed',
        droppedItems: 0,
        droppedBytes: 0
      }
    ])
  ]
  const ack = jest.fn(async token => ok(receipt(token === 'tail')))
  const control = retainedControl(async () => ok(queue.shift()), ack)
  const first = await control.claimRetainedV1({ maxItems: 1, maxBytes: 1000 })
  expect(ack).not.toHaveBeenCalled()
  expect((await control.claimRetainedV1()).claimToken).toBe('one')
  expect(first.records.map(record => record.ordinal)).toEqual([1])
  expect((await first.acknowledge()).disposed).toBe(false)
  const second = await control.claimRetainedV1()
  expect(second.records.map(record => record.ordinal)).toEqual([2])
  expect(ack.mock.calls.map(call => call[0])).toEqual(['one'])
  await second.acknowledge()
  const tail = await control.claimRetainedV1()
  expect(tail.records.map(record => record.ordinal)).toEqual([3])
  expect((await tail.acknowledge()).disposed).toBe(true)
  expect(ack.mock.calls.map(call => call[0])).toEqual(['one', 'two', 'tail'])
})

test('retries an uncertain ACK with the same token before another prepare and ignores a stale duplicate', async () => {
  let count = 0
  const prepare = jest.fn(async () => ok(prepared(count === 0 ? 'one' : 'two', [value(count + 1)])))
  const ack = jest.fn(async () => {
    if (count++ === 0) throw Error('lost reply')
    return ok(receipt(false))
  })
  const control = retainedControl(prepare, ack)
  const first = await control.claimRetainedV1()
  await expect(first.acknowledge()).rejects.toMatchObject({ code: 'platform.failure' })
  expect((await control.claimRetainedV1()).claimToken).toBe('one')
  expect(prepare).toHaveBeenCalledTimes(1)
  await first.acknowledge()
  const second = await control.claimRetainedV1()
  expect(second.claimToken).toBe('two')
  await first.acknowledge()
  expect(ack.mock.calls.map(call => call[0])).toEqual(['one', 'one'])
})

test('rejects a reordered prefix without acknowledging it', async () => {
  const queue = [prepared('one', [value(3)]), prepared('two', [value(2)])]
  const ack = jest.fn(async () => ok(receipt(false)))
  const control = retainedControl(async () => ok(queue.shift()), ack)
  await (await control.claimRetainedV1()).acknowledge()
  await expect(control.claimRetainedV1()).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(ack).toHaveBeenCalledTimes(1)
})

test('joins concurrent prepare calls even if the first caller abandons its promise', async () => {
  let release
  const prepare = jest.fn(
    () =>
      new Promise(resolve => {
        release = () => resolve(ok(prepared('stable', [value(1)])))
      })
  )
  const ack = jest.fn(async () => ok(receipt(true)))
  const control = retainedControl(prepare, ack)
  const abandoned = control.claimRetainedV1()
  const waiting = control.claimRetainedV1()
  release()
  const [first, second] = await Promise.all([abandoned, waiting])
  expect(first).toBe(second)
  expect(prepare).toHaveBeenCalledTimes(1)
  expect(ack).not.toHaveBeenCalled()
  expect((await control.claimRetainedV1()).claimToken).toBe('stable')
})

test('joins a pending acknowledgement and does not prepare a tail until its receipt', async () => {
  let finishAck
  const queue = [prepared('one', [value(1)]), prepared('tail', [value(2)])]
  const prepare = jest.fn(async () => ok(queue.shift()))
  const ack = jest.fn(
    () =>
      new Promise(resolve => {
        finishAck = () => resolve(ok(receipt(false)))
      })
  )
  const control = retainedControl(prepare, ack)
  const first = await control.claimRetainedV1()
  const abandonedAck = first.acknowledge()
  const duplicate = first.acknowledge()
  const next = control.claimRetainedV1()
  expect(prepare).toHaveBeenCalledTimes(1)
  finishAck()
  await Promise.all([abandonedAck, duplicate])
  expect((await next).claimToken).toBe('tail')
  expect(ack).toHaveBeenCalledTimes(1)
})

test('keeps the same disclosed handoff after malformed ACK and rejects replayed data for its token', async () => {
  let attempts = 0
  const prepare = jest.fn(async () => ok(prepared('one', [value(1)])))
  const ack = jest.fn(async () => ok(++attempts === 1 ? { disposed: 'unknown' } : receipt(false)))
  const control = retainedControl(prepare, ack)
  const first = await control.claimRetainedV1()
  await expect(first.acknowledge()).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(await control.claimRetainedV1()).toBe(first)
  await first.acknowledge()
  await expect(control.claimRetainedV1()).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(ack.mock.calls.map(call => call[0])).toEqual(['one', 'one'])
})

test('a lost prepare reply and malformed prefix do not authorize an ACK', async () => {
  let calls = 0
  const ack = jest.fn()
  const control = retainedControl(async () => {
    if (++calls === 1) throw Error('lost reply')
    return ok(prepared('one', [{ ...value(1), valueB64: 7 }]))
  }, ack)
  await expect(control.claimRetainedV1()).rejects.toMatchObject({ code: 'platform.failure' })
  await expect(control.claimRetainedV1()).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(ack).not.toHaveBeenCalled()
})

test('an empty retained handoff can retry cleanup with the same token', async () => {
  const empty = prepared('empty', [])
  const prepare = jest.fn(async () => ok(empty))
  const ack = jest
    .fn()
    .mockResolvedValueOnce(ok(receipt(false)))
    .mockResolvedValueOnce(ok(receipt(true)))
  const control = retainedControl(prepare, ack)
  const first = await control.claimRetainedV1()
  expect((await first.acknowledge()).disposed).toBe(false)
  const retry = await control.claimRetainedV1()
  expect(retry.claimToken).toBe('empty')
  expect(retry.records).toEqual([])
  expect((await retry.acknowledge()).disposed).toBe(true)
  expect(ack.mock.calls.map(call => call[0])).toEqual(['empty', 'empty'])
})

test('rejects invalid common bounds without preparing or acknowledging', async () => {
  const prepare = jest.fn()
  const ack = jest.fn()
  const control = retainedControl(prepare, ack)
  for (const request of [{ maxItems: 0 }, { maxBytes: 2 ** 31 }, { maxBytes: 1.5 }]) {
    await expect(control.claimRetainedV1(request)).rejects.toMatchObject({ code: 'argument.invalid' })
  }
  expect(prepare).not.toHaveBeenCalled()
  expect(ack).not.toHaveBeenCalled()
})

test('oversized mobile claim text is rejected without ACK and remains retryable', async () => {
  const prepare = jest.fn(async () => ok({ ...prepared('large', [value(1)]), extra: 'é'.repeat(600000) }))
  const ack = jest.fn()
  const control = createNativeContinuationControl(
    { prepareClaim: prepare, acknowledgeClaim: ack },
    { format: 'mobile', hostDomain: 'ubm-mobile', scope: 'mobile' }
  )
  await expect(control.claim()).rejects.toMatchObject({ code: 'bytes.too-large' })
  expect(ack).not.toHaveBeenCalled()
  await expect(control.claim()).rejects.toMatchObject({ code: 'bytes.too-large' })
  expect(prepare).toHaveBeenCalledTimes(2)
})

test('V2 recovery is public only as one complete capability and cannot acknowledge its prefix before every recovery is read and acknowledged', async () => {
  const record = recovery()
  const prepare = jest.fn(async () => ok(prepared('v2-token', [record])))
  const acknowledge = jest.fn(async () => ok(receipt(true)))
  const read = jest
    .fn()
    .mockResolvedValueOnce(ok({ offset: '0', nextOffset: '2', bytesB64: 'AQI=', done: false }))
    .mockResolvedValueOnce(ok({ offset: '2', nextOffset: '3', bytesB64: 'Aw==', done: true }))
  const acknowledgeRecovery = jest.fn(async () => ok({}))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: prepare,
    acknowledgeRetainedClaimV2: acknowledge,
    readPeerFailureRecovery: read,
    acknowledgePeerFailureRecovery: acknowledgeRecovery
  })
  const prefix = await control.claimRetainedV2()
  expect(prefix.recoveries).toHaveLength(1)
  await expect(prefix.acknowledge()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
  const first = await prefix.recoveries[0].read(0)
  expect([...first.bytes]).toEqual([1, 2])
  await expect(prefix.recoveries[0].acknowledge()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
  const last = await prefix.recoveries[0].read(first.nextOffset)
  expect([...last.bytes]).toEqual([3])
  await prefix.recoveries[0].acknowledge()
  await prefix.acknowledge()
  expect(acknowledgeRecovery).toHaveBeenCalledTimes(1)
  expect(acknowledge).toHaveBeenCalledWith('v2-token')
  expect(read.mock.calls.map(call => call[0])).toEqual([JSON.stringify(record), JSON.stringify(record)])
  expect(acknowledgeRecovery).toHaveBeenCalledWith(JSON.stringify(record))
})

test('V2 does not cache a no-token prepare so a later source prefix can be claimed', async () => {
  const record = recovery()
  const prepare = jest
    .fn()
    .mockResolvedValueOnce(ok(prepared(undefined, [])))
    .mockResolvedValueOnce(ok(prepared('later-token', [record])))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: prepare,
    acknowledgeRetainedClaimV2: async () => ok(receipt(true)),
    readPeerFailureRecovery: async () => ok({ offset: '0', nextOffset: '1', bytesB64: 'AQ==', done: true }),
    acknowledgePeerFailureRecovery: async () => ok({})
  })
  const empty = await control.claimRetainedV2()
  expect(empty.claimToken).toBeUndefined()
  expect((await control.claimRetainedV2()).claimToken).toBe('later-token')
  expect(prepare).toHaveBeenCalledTimes(2)
})

test('V2 coalesces recovery acknowledgement attempts and retries failed or malformed replies', async () => {
  const record = recovery('1')
  let rejectFirst
  const acknowledgeRecovery = jest
    .fn()
    .mockImplementationOnce(
      () =>
        new Promise((_, reject) => {
          rejectFirst = reject
        })
    )
    .mockResolvedValueOnce(ok({ unexpected: true }))
    .mockResolvedValueOnce(ok({}))
  const acknowledge = jest.fn(async () => ok(receipt(true)))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: async () => ok(prepared('v2-token', [record])),
    acknowledgeRetainedClaimV2: acknowledge,
    readPeerFailureRecovery: async () => ok({ offset: '0', nextOffset: '1', bytesB64: 'AQ==', done: true }),
    acknowledgePeerFailureRecovery: acknowledgeRecovery
  })
  const prefix = await control.claimRetainedV2()
  await prefix.recoveries[0].read(0)
  const first = prefix.recoveries[0].acknowledge()
  const joined = prefix.recoveries[0].acknowledge()
  expect(acknowledgeRecovery).toHaveBeenCalledTimes(1)
  rejectFirst(Error('lost recovery ACK'))
  await expect(first).rejects.toMatchObject({ code: 'platform.failure' })
  await expect(joined).rejects.toMatchObject({ code: 'platform.failure' })
  await expect(prefix.recoveries[0].acknowledge()).rejects.toMatchObject({ code: 'protocol.malformed' })
  await prefix.recoveries[0].acknowledge()
  await prefix.acknowledge()
  expect(acknowledgeRecovery).toHaveBeenCalledTimes(3)
  expect(acknowledge).toHaveBeenCalledTimes(1)
})

test('V2 preserves a retained recovery after a failed read and cannot source-ack it until the retry completes', async () => {
  const record = recovery('1')
  const read = jest
    .fn()
    .mockRejectedValueOnce(Error('stale native route'))
    .mockResolvedValueOnce(ok({ offset: '0', nextOffset: '1', bytesB64: 'AQ==', done: true }))
  const acknowledgeRecovery = jest.fn(async () => ok({}))
  const acknowledge = jest.fn(async () => ok(receipt(true)))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: async () => ok(prepared('v2-token', [record])),
    acknowledgeRetainedClaimV2: acknowledge,
    readPeerFailureRecovery: read,
    acknowledgePeerFailureRecovery: acknowledgeRecovery
  })
  const prefix = await control.claimRetainedV2()
  await expect(prefix.recoveries[0].read(0)).rejects.toMatchObject({ code: 'platform.failure' })
  await expect(prefix.acknowledge()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
  const page = await prefix.recoveries[0].read(0)
  expect(page.done).toBe(true)
  await prefix.recoveries[0].acknowledge()
  await prefix.acknowledge()
  expect(read).toHaveBeenCalledTimes(2)
  expect(acknowledgeRecovery).toHaveBeenCalledTimes(1)
  expect(acknowledge).toHaveBeenCalledTimes(1)
})

test('V2 rejects an early done page and keeps its exact offset retryable', async () => {
  const record = recovery()
  const read = jest
    .fn()
    .mockResolvedValueOnce(ok({ offset: '0', nextOffset: '1', bytesB64: 'AQ==', done: true }))
    .mockResolvedValueOnce(ok({ offset: '0', nextOffset: '3', bytesB64: 'AQID', done: true }))
  const acknowledgeRecovery = jest.fn(async () => ok({}))
  const acknowledge = jest.fn(async () => ok(receipt(true)))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: async () => ok(prepared('v2-token', [record])),
    acknowledgeRetainedClaimV2: acknowledge,
    readPeerFailureRecovery: read,
    acknowledgePeerFailureRecovery: acknowledgeRecovery
  })
  const prefix = await control.claimRetainedV2()
  await expect(prefix.recoveries[0].read(0)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(prefix.acknowledge()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
  const page = await prefix.recoveries[0].read(0)
  expect(page).toMatchObject({ offset: 0, nextOffset: 3, done: true })
  await prefix.recoveries[0].acknowledge()
  await prefix.acknowledge()
  expect(read.mock.calls.map(call => call[1])).toEqual(['0', '0'])
  expect(acknowledgeRecovery).toHaveBeenCalledTimes(1)
  expect(acknowledge).toHaveBeenCalledTimes(1)
})

test('V2 refuses malformed recovery bounds before it can authorize a retained source prefix', async () => {
  for (const record of [recovery('0'), recovery('9007199254740992'), recovery('1', 0), recovery('1', 65537)]) {
    const prepare = jest.fn(async () => ok(prepared('v2-token', [record])))
    const acknowledge = jest.fn(async () => ok(receipt(true)))
    const control = createNativeContinuationControl({
      prepareClaim: async () => ok(prepared('legacy', [])),
      acknowledgeClaim: async () => ok(receipt(true)),
      prepareRetainedClaimV2: prepare,
      acknowledgeRetainedClaimV2: acknowledge,
      readPeerFailureRecovery: jest.fn(),
      acknowledgePeerFailureRecovery: jest.fn()
    })
    await expect(control.claimRetainedV2()).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(acknowledge).not.toHaveBeenCalled()
  }
})

test('V2 refuses an overrun recovery page without consuming its offset', async () => {
  const record = recovery('3')
  const read = jest
    .fn()
    .mockResolvedValueOnce(ok({ offset: '0', nextOffset: '4', bytesB64: 'AQIDBA==', done: true }))
    .mockResolvedValueOnce(ok({ offset: '0', nextOffset: '3', bytesB64: 'AQID', done: true }))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: async () => ok(prepared('v2-token', [record])),
    acknowledgeRetainedClaimV2: async () => ok(receipt(true)),
    readPeerFailureRecovery: read,
    acknowledgePeerFailureRecovery: async () => ok({})
  })
  const prefix = await control.claimRetainedV2()
  await expect(prefix.recoveries[0].read(0)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(prefix.recoveries[0].read(0)).resolves.toMatchObject({ nextOffset: 3, done: true })
  expect(read.mock.calls.map(call => call[1])).toEqual(['0', '0'])
})

test('V2 retries a lost final source ACK on the same disclosed prefix without repeating recovery acknowledgement', async () => {
  const record = recovery('1')
  const prepare = jest.fn(async () => ok(prepared('v2-token', [record])))
  const read = jest.fn(async () => ok({ offset: '0', nextOffset: '1', bytesB64: 'AQ==', done: true }))
  const acknowledgeRecovery = jest.fn(async () => ok({}))
  const acknowledge = jest
    .fn()
    .mockRejectedValueOnce(Error('lost final reply'))
    .mockResolvedValueOnce(ok(receipt(true)))
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: prepare,
    acknowledgeRetainedClaimV2: acknowledge,
    readPeerFailureRecovery: read,
    acknowledgePeerFailureRecovery: acknowledgeRecovery
  })
  const prefix = await control.claimRetainedV2()
  await prefix.recoveries[0].read(0)
  await prefix.recoveries[0].acknowledge()
  await expect(prefix.acknowledge()).rejects.toMatchObject({ code: 'platform.failure' })
  expect(await control.claimRetainedV2()).toBe(prefix)
  await expect(prefix.acknowledge()).resolves.toMatchObject({ disposed: true })
  expect(prepare).toHaveBeenCalledTimes(1)
  expect(acknowledgeRecovery).toHaveBeenCalledTimes(1)
  expect(acknowledge).toHaveBeenCalledTimes(2)
})

test('V2 refuses a partial native module before it prepares a source prefix', async () => {
  const prepare = jest.fn()
  const control = createNativeContinuationControl({
    prepareClaim: async () => ok(prepared('legacy', [])),
    acknowledgeClaim: async () => ok(receipt(true)),
    prepareRetainedClaimV2: prepare,
    acknowledgeRetainedClaimV2: async () => ok(receipt(true))
  })
  expect(control.claimRetainedV2).toBeUndefined()
  expect(prepare).not.toHaveBeenCalled()
})
