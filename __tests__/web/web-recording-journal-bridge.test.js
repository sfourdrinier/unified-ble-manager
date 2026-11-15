const { createWebRecordingJournalBridge } = require('../../src/web/web-recording-journal-bridge')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { recordingDatabase, reserveJournal } = require('./web-continuation-recording-controller-helper')
const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebRetainedRecordingPump } = require('../../src/core/web-retained-recording-pump')

const declaration = {
  id: 'bridge',
  maxBytes: 1048576,
  maxRecords: 10,
  producerIncarnation: 'producer',
  collectionId: '1',
  sourceGeneration: 'generation',
  ownerEpoch: '1',
  context: {
    kind: 'ordered-recording',
    version: 1,
    peerId: 'origin-peer',
    connectionGeneration: 'connection',
    databaseGeneration: 'graph',
    maxItems: 8,
    maxBytes: 1024,
    maxControls: 4,
    members: [
      {
        memberId: '0',
        publicMemberId: 'heart',
        selector: {
          serviceUuid: '180d',
          serviceOccurrence: '0',
          characteristicUuid: '2a37',
          characteristicOccurrence: '0'
        },
        delivery: null,
        preference: null
      }
    ]
  }
}

function fixture(wrap = journal => journal) {
  const db = recordingDatabase()
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const source = ingress.registerRetained(new Map([[characteristic, '0']]), 8, 1024, { membership: true })
  const authority = { current: () => true, release: jest.fn(async () => {}) }
  let journal
  const bridge = createWebRecordingJournalBridge(db, async memberEpochs => {
    journal = await reserveJournal(db, { ...declaration, memberEpochs })
    return declaration
  }, async () => authority, () => wrap(journal))
  return { db, ingress, characteristic, source, authority, bridge }
}

test('declares the actual source membership and replays copied values and terminal with identity', async () => {
  const f = fixture()
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  f.ingress.admit(f.characteristic, Uint8Array.from([0, 255]))
  f.source.retire('connection-lost')
  const pump = new WebRetainedRecordingPump(f.source, f.bridge)
  while ((await pump.runOnce()) !== 'empty') {}
  await f.bridge.seal()
  const controller = await createWebContinuationRecordingController({ database: f.db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('bridge', { maxItems: 8, maxBytes: 10000 })
  expect(batch.records.map(row => row.metadata.deliveryId)).toEqual(['1', '2'])
  expect(batch.records[0]).toMatchObject({
    metadata: { sourceGeneration: { kind: 'opaque', value: 'generation' } },
    record: {
      kind: 'value',
      ordinal: '1',
      member: '0',
      membership: { member: '0', epoch: '1' },
      bytes: Uint8Array.from([0, 255]),
      delivery: 'unknown',
      provenance: 'read-or-notification'
    }
  })
  expect(batch.records[1].record).toMatchObject({
    kind: 'terminal',
    reason: { kind: 'connection-lost' },
    tailUncertain: true
  })
  expect(f.db.rows('recordings')[0].memberEpochs).toEqual([{ member: '0', epoch: '1' }])
})

test('reconciles the exact committed identity after a lost commit reply before acknowledging source', async () => {
  const f = fixture(journal => {
    return {
      ...journal,
      commitWithDurability: async delivery => {
        await journal.commitWithDurability(delivery)
        throw new Error('lost reply')
      }
    }
  })
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  f.ingress.admit(f.characteristic, Uint8Array.from([9]))
  const acknowledged = jest.fn()
  await new WebRetainedRecordingPump(f.source, { ...f.bridge, acknowledged }).runOnce()
  expect(f.source.peek()).toBeNull()
  expect(f.db.rows('records')).toHaveLength(1)
  expect(acknowledged).toHaveBeenCalledWith(expect.objectContaining({ id: '1', journalOrdinal: 1 }))
  expect(f.bridge.lastCommit()).toMatchObject({
    evidence: 'reconciled',
    durability: null,
    reconciliationDurability: expect.any(String)
  })
})

test('retained marker credit orders the marker before the write with its actual source generation', async () => {
  const { WebOrderedMarkerTransactions } = require('../../src/web/web-ordered-marker')
  const f = fixture()
  const target = {
    writeValueWithResponse: jest.fn(() => Promise.resolve()),
    writeValueWithoutResponse: jest.fn(() => Promise.resolve())
  }
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  const marker = new WebOrderedMarkerTransactions(f.source, f.source, '1', 'connection', 'graph', {
    kind: 'opaque',
    value: 'generation'
  })
  const receipt = marker.write('0', target, Uint8Array.from([7]), 'with-response', null)
  expect(receipt.marker).toEqual({ state: 'admitted', ordinal: '1' })
  expect(f.source.peek().record.sourceGeneration).toEqual({ kind: 'opaque', value: 'generation' })
  await new WebRetainedRecordingPump(f.source, f.bridge).runOnce()
  expect(f.db.rows('records')[0].record).toEqual({ kind: 'marker', markerId: '0', ordinal: '1' })
  expect(target.writeValueWithResponse).toHaveBeenCalledTimes(1)
})

test('refuses a marker from a different source rather than persisting it under this generation', async () => {
  const f = fixture()
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  await expect(
    f.bridge.commit({
      id: '1',
      record: {
        kind: 'marker',
        collectionId: '1',
        connectionGeneration: 'connection',
        databaseGeneration: 'graph',
        sourceGeneration: { kind: 'opaque', value: 'other' },
        sourceMembership: null,
        markerId: '0',
        ordinal: '1'
      }
    })
  ).rejects.toThrow()
  expect(f.db.rows('records')).toHaveLength(0)
})

test('supervisor does not acknowledge a committed head after its authority fence is lost', async () => {
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  let source,
    wake,
    current = true
  const owner = {
    reserve: () => ({ memberships: () => [{ member: '0', epoch: '1' }] }),
    isCurrent: () => true,
    release: async () => {},
    install: callback => {
      wake = callback
      source = ingress.registerRetained(new Map([[characteristic, '0']]), 8, 1024, { membership: true })
      return { source, leases: [{ id: 'heart', enable: async () => {}, release: async () => {} }] }
    }
  }
  const journal = {
    prepare: async () => {},
    open: async () => {},
    seal: async () => {},
    releaseAuthority: async () => {},
    commit: async () => {
      current = false
    },
    retire: jest.fn(async () => {}),
    canAcknowledge: () => current,
    classifyFailure: () => 'failure',
    isDefinitiveFailure: () => true
  }
  const supervisor = new WebOrderedRecordingSupervisor(owner, journal)
  await supervisor.start()
  ingress.admit(characteristic, Uint8Array.from([1]))
  wake()
  await supervisor.retry()
  expect(source.peek()).toMatchObject({ id: '1', record: { kind: 'value' } })
  expect(journal.retire).not.toHaveBeenCalled()
})

test('classifies a proven absent quota transaction as capacity and retains the exact head', async () => {
  const { webRecordingContractError: contractError } = require('../../src/web/web-recording-errors')
  const f = fixture(journal => {
    return {
      ...journal,
      commitWithDurability: async () => {
        throw contractError('stream.quota', 'restoration', 'web-recording.capacity')
      }
    }
  })
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  f.ingress.admit(f.characteristic, Uint8Array.from([1]))
  let failure
  try {
    await new WebRetainedRecordingPump(f.source, f.bridge).runOnce()
  } catch (error) {
    failure = error
  }
  expect(f.bridge.isDefinitiveFailure(failure)).toBe(true)
  expect(f.bridge.classifyFailure(failure)).toBe('capacity')
  expect(f.source.peek()).toMatchObject({ id: '1' })
})

test('does not classify a host-created typed quota as a library capacity failure', async () => {
  const { contractError } = require('../../src/backend-contract/errors')
  const f = fixture(journal => ({ ...journal, commitWithDurability: async () => { throw contractError('stream.quota', 'restoration', 'secret') } }))
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  f.ingress.admit(f.characteristic, Uint8Array.from([1]))
  const failure = await new WebRetainedRecordingPump(f.source, f.bridge).runOnce().catch(value => value)
  expect(f.bridge.isDefinitiveFailure(failure)).toBe(true)
  expect(f.bridge.classifyFailure(failure)).toBe('failure')
})

test('does not acknowledge a delivery when exact reconciliation is unknown', async () => {
  const f = fixture(journal => {
    return {
      ...journal,
      commitWithDurability: async () => {
        throw new Error('lost completion')
      },
      reconcile: async () => ({ outcome: 'unknown', durability: 'default' })
    }
  })
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  f.ingress.admit(f.characteristic, Uint8Array.from([]))
  await expect(new WebRetainedRecordingPump(f.source, f.bridge).runOnce()).rejects.toThrow('lost completion')
  expect(f.source.peek()).toMatchObject({ id: '1', record: { value: Uint8Array.from([]) } })
  expect(f.db.rows('records')).toHaveLength(0)
})

test('preserves full-width source ordinals and exact known loss without inventing a control ordinal', async () => {
  const f = fixture()
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  await f.bridge.commit({
    id: '1',
    record: {
      kind: 'value',
      memberId: '0',
      ordinal: '18446744073709551615',
      value: Uint8Array.from([]),
      sourceMembership: { member: '0', epoch: '1' },
      delivery: 'unknown',
      provenance: 'read-or-notification'
    }
  })
  await f.bridge.retire('1')
  await f.bridge.commit({ id: '2', record: { kind: 'loss', lostValues: 1, lostBytes: 3 } })
  const controller = await createWebContinuationRecordingController({ database: f.db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('bridge', { maxItems: 8, maxBytes: 10000 })
  expect(batch.records[0].record).toMatchObject({ ordinal: '18446744073709551615', bytes: Uint8Array.from([]) })
  expect(batch.records[1].record).toEqual({
    kind: 'loss',
    scope: { kind: 'group' },
    unit: 'values',
    count: '1',
    bytes: '3'
  })
})

test('retains the original failure and failed reconciliation while the exact outcome remains unknown', async () => {
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const original = new WebRecordingReadinessError(new Error('temporary'))
  const readFailure = new Error('read aborted')
  let reads = 0, writes = 0
  const f = fixture(journal => ({ ...journal,
    commitWithDurability: async () => { writes++; throw original },
    reconcile: async () => {
      reads++
      if (reads === 1) throw readFailure
      if (reads === 2) return { outcome: 'unknown', durability: 'default' }
      return { outcome: 'absent', durability: 'default' }
    }
  }))
  await f.bridge.prepare()
  await f.bridge.open(f.source)
  f.ingress.admit(f.characteristic, Uint8Array.of(42))
  const delivery = f.source.peek()
  const first = await f.bridge.commit(delivery).catch(error => error)
  expect(first.errors).toEqual([original, readFailure])
  const second = await f.bridge.commit(delivery).catch(error => error)
  expect(second.errors).toEqual([original, readFailure])
  const absent = await f.bridge.commit(delivery).catch(error => error)
  expect(f.bridge.isDefinitiveFailure(absent)).toBe(true)
  expect(require('../../src/web/web-recording-journal-bridge').webRecordingOriginalFailure(absent)).toBe(original)
  expect(writes).toBe(1)
  expect(f.source.peek()).toEqual(delivery)
})

test('retains a late reconciliation rejection alongside its timeout and original failure', async () => {
  jest.useFakeTimers()
  try {
    const original = new Error('lost commit')
    const late = new DOMException('private read detail', 'AbortError')
    let rejectRead, reads = 0, writes = 0
    const held = new Promise((_resolve, reject) => { rejectRead = reject })
    const f = fixture(journal => ({ ...journal,
      commitWithDurability: async () => { writes++; throw original },
      reconcile: async () => {
        reads++
        return reads === 1 ? held : { outcome: 'unknown', durability: 'default' }
      }
    }))
    await f.bridge.prepare()
    await f.bridge.open(f.source)
    f.ingress.admit(f.characteristic, Uint8Array.of(42))
    const delivery = f.source.peek()
    const firstPromise = f.bridge.commit(delivery).catch(error => error)
    await jest.advanceTimersByTimeAsync(5000)
    const first = await firstPromise
    expect(first.errors[0]).toBe(original)
    expect(first.errors[1].message).toBe('Web recording transaction outcome remains unknown')
    rejectRead(late)
    await Promise.resolve()
    await Promise.resolve()
    const retry = await f.bridge.commit(delivery).catch(error => error)
    expect(retry.errors).toEqual([original, first.errors[1], { name: 'AbortError' }])
    expect(JSON.stringify(retry.errors)).not.toContain('private read detail')
    expect(writes).toBe(1)
    expect(f.source.peek()).toEqual(delivery)
  } finally {
    jest.useRealTimers()
  }
})

test('a retry waiting on a timed-out read reports its late rejection once and redacted', async () => {
  jest.useFakeTimers()
  try {
    const original = new Error('lost commit')
    const late = new DOMException('private read detail', 'AbortError')
    let rejectRead, writes = 0
    const held = new Promise((_resolve, reject) => { rejectRead = reject })
    const f = fixture(journal => ({ ...journal,
      commitWithDurability: async () => { writes++; throw original },
      reconcile: async () => held
    }))
    await f.bridge.prepare()
    await f.bridge.open(f.source)
    f.ingress.admit(f.characteristic, Uint8Array.of(42))
    const delivery = f.source.peek()
    const firstPromise = f.bridge.commit(delivery).catch(error => error)
    await jest.advanceTimersByTimeAsync(5000)
    const first = await firstPromise
    const retryPromise = f.bridge.commit(delivery).catch(error => error)
    rejectRead(late)
    const retry = await retryPromise
    expect(retry.errors).toEqual([original, first.errors[1], { name: 'AbortError' }])
    expect(writes).toBe(1)
  } finally {
    jest.useRealTimers()
  }
})
