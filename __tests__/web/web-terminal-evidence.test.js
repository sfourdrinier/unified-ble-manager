const { createWebTerminalEvidenceSource } = require('../../src/web/web-terminal-evidence')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')

function locks() {
  return {
    request: async (_name, _options, callback) => {
      await callback({})
    }
  }
}

function crashableLocks() {
  const crashes = new Set()
  const held = new Set()
  return {
    request: async (name, _options, callback) => {
      if (held.has(name)) return callback(null)
      held.add(name)
      let crash
      const crashed = new Promise(resolve => {
        crash = resolve
      })
      crashes.add(crash)
      try {
        await Promise.race([Promise.resolve(callback({})), crashed])
      } finally {
        crashes.delete(crash)
        held.delete(name)
      }
    },
    crash: () => {
      held.clear()
      ;[...crashes].forEach(resolve => resolve())
    }
  }
}

async function digest(value) {
  let total = 0
  for (const character of value) total = (total * 31 + character.charCodeAt(0)) >>> 0
  return total.toString(16).padStart(64, '0')
}

async function digestBytes(value) {
  return digest(Array.from(value, byte => String.fromCharCode(byte)).join(''))
}

function uint32GetAllDatabase() {
  const database = transactionalDatabase()
  const counts = []
  const transaction = database.transaction.bind(database)
  database.transaction = (...args) => {
    const tx = transaction(...args)
    const objectStore = tx.objectStore.bind(tx)
    tx.objectStore = name => {
      const store = objectStore(name)
      return {
        ...store,
        getAll: (range, count) => {
          counts.push(count)
          if (!Number.isInteger(count) || count < 0 || count > 0xffffffff)
            throw new TypeError('IndexedDB getAll count must be an unsigned 32-bit integer')
          return store.getAll(range, count)
        }
      }
    }
    return tx
  }
  return { database, counts }
}

describe.each(['reserve', 'release-reservation', 'acknowledge'])('persisted quota validation: %s', operation => {
  test.each([
    ['string byte count', { reservedBytes: '2' }],
    ['negative byte count', { reservedBytes: -1 }],
    ['fractional byte count', { reservedBytes: 1.5 }],
    ['unsafe byte count', { reservedBytes: Number.MAX_SAFE_INTEGER + 1 }],
    ['over-capacity byte count', { reservedBytes: 5 }],
    ['invalid capacity', { maximumBytes: 0 }],
    ['unsupported version', { version: 2 }],
    ['wrong state key', { key: 'other' }]
  ])('rejects %s without changing saved evidence or quota, and permits retry', async (_name, corruption) => {
    const database = transactionalDatabase()
    const options = { database, locks: locks(), digest, digestBytes, maximumRetainedBytes: 4 }
    const source = createWebTerminalEvidenceSource(options)
    const request = {
      producer: 'scan',
      maximumBytes: 2,
      formatVersion: 1,
      scope: { managerId: 'm', operationId: 'quota-validation', incarnation: 'i' }
    }
    const reservation = await source.reserveTerminalEvidence(request)
    let action
    if (operation === 'reserve') {
      action = () =>
        source.reserveTerminalEvidence({
          ...request,
          scope: { ...request.scope, operationId: 'second' }
        })
    } else if (operation === 'release-reservation') {
      action = () => reservation.release()
    } else {
      const canonicalBytes = new Uint8Array([3])
      const receipt = await reservation.commit({ canonicalBytes, digest: await digestBytes(canonicalBytes) })
      await reservation.release()
      action = () => source.releaseTerminalEvidence(receipt.ref)
    }
    const validQuota = database.rows('terminalEvidenceState')[0]
    database.mutate('terminalEvidenceState', row => Object.assign(row, corruption))
    const savedEvidence = database.rows('terminalEvidence')
    const savedQuota = database.rows('terminalEvidenceState')
    await expect(action()).rejects.toThrow()
    expect(database.rows('terminalEvidence')).toEqual(savedEvidence)
    expect(database.rows('terminalEvidenceState')).toEqual(savedQuota)
    database.mutate('terminalEvidenceState', row => Object.assign(row, validQuota))
    const retry = await action()
    if (operation === 'reserve') await retry.release()
    await reservation.release()
    expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 0 })])
  })
})

test('persists a preactivated reservation and exact evidence before it reports a commit', async () => {
  const database = transactionalDatabase()
  const source = createWebTerminalEvidenceSource({
    database,
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 4
  })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'scan',
    maximumBytes: 4,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'o', incarnation: 'i' }
  })
  expect(database.rows('terminalEvidence')).toEqual(
    expect.arrayContaining([expect.objectContaining({ state: 'reserved', maximumBytes: 4 })])
  )
  const canonicalBytes = new Uint8Array([1, 2])
  const evidenceDigest = await digestBytes(canonicalBytes)
  await expect(reservation.commit({ canonicalBytes, digest: evidenceDigest })).resolves.toMatchObject({
    byteLength: 2,
    digest: evidenceDigest,
    durability: 'committed'
  })
  expect(database.rows('terminalEvidence')).toEqual(
    expect.arrayContaining([expect.objectContaining({ state: 'committed', bytes: [1, 2] })])
  )
  await reservation.release()
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 4 })])
  await expect(source.readTerminalEvidence(database.rows('terminalEvidence')[0].ref, 4)).resolves.toMatchObject({
    kind: 'committed',
    canonicalBytes: new Uint8Array([1, 2])
  })
  await expect(source.readTerminalEvidence('missing', 4)).resolves.toEqual({ kind: 'missing' })
})

test('reclaims a crashed reserved row only after its lock is released and preserves staged recovery', async () => {
  const database = transactionalDatabase()
  const webLocks = crashableLocks()
  const options = { database, locks: webLocks, digest, digestBytes, maximumRetainedBytes: 3 }
  const request = {
    producer: 'subscription',
    maximumBytes: 3,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'ack-state', incarnation: 'i' }
  }
  const reservation = await createWebTerminalEvidenceSource(options).reserveTerminalEvidence(request)
  const ref = database.rows('terminalEvidence')[0].ref

  await expect(createWebTerminalEvidenceSource(options).releaseTerminalEvidence(ref)).rejects.toThrow(
    'web-terminal-evidence.release-busy'
  )
  expect(database.rows('terminalEvidence')).toEqual([expect.objectContaining({ ref, state: 'reserved' })])
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])

  webLocks.crash()
  const recovered = createWebTerminalEvidenceSource(options)
  database.abortNext(new Error('injected-reserved-retirement-abort'))
  await expect(recovered.releaseTerminalEvidence(ref)).rejects.toThrow('injected-reserved-retirement-abort')
  expect(database.rows('terminalEvidence')).toEqual([expect.objectContaining({ ref, state: 'reserved' })])
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])
  await recovered.releaseTerminalEvidence(ref)
  expect(database.rows('terminalEvidence')).toEqual([])
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 0 })])

  const stagedReservation = await createWebTerminalEvidenceSource(options).reserveTerminalEvidence({
    ...request,
    scope: { ...request.scope, operationId: 'ack-staged' }
  })
  const stagedRef = database.rows('terminalEvidence')[0].ref
  const bytes = new Uint8Array([3])
  await stagedReservation.commit({ canonicalBytes: bytes, digest: await digestBytes(bytes) })
  database.mutate('terminalEvidence', row => {
    row.state = 'staged'
  })
  await stagedReservation.release()
  await expect(createWebTerminalEvidenceSource(options).releaseTerminalEvidence(stagedRef)).rejects.toThrow(
    'web-terminal-evidence.release-state'
  )
  expect(database.rows('terminalEvidence')).toEqual([
    expect.objectContaining({ ref: stagedRef, state: 'staged', bytes: [3] })
  ])
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])
  await createWebTerminalEvidenceSource(options).retryPendingTerminalEvidence()
  await expect(createWebTerminalEvidenceSource(options).readTerminalEvidence(stagedRef, 3)).resolves.toMatchObject({
    kind: 'committed',
    canonicalBytes: new Uint8Array([3])
  })
  await createWebTerminalEvidenceSource(options).releaseTerminalEvidence(stagedRef)
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 0 })])
  void reservation
})

test('does not silently use volatile storage when exclusive Web Locks are absent', async () => {
  const source = createWebTerminalEvidenceSource({
    database: transactionalDatabase(),
    digest: async () => 'a'.repeat(64),
    digestBytes,
    maximumRetainedBytes: 1
  })
  await expect(
    source.reserveTerminalEvidence({
      producer: 'scan',
      maximumBytes: 1,
      formatVersion: 1,
      scope: { managerId: 'm', operationId: 'o', incarnation: 'i' }
    })
  ).rejects.toThrow('web-terminal-evidence.web-lock')
})

test('replays the exact committed receipt after source recreation', async () => {
  const database = transactionalDatabase()
  const webLocks = crashableLocks()
  const options = { database, locks: webLocks, digest, digestBytes, maximumRetainedBytes: 4 }
  const request = {
    producer: 'scan',
    maximumBytes: 4,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'restart', incarnation: 'i' }
  }
  const first = createWebTerminalEvidenceSource(options)
  const initial = await first.reserveTerminalEvidence(request)
  const canonicalBytes = new Uint8Array([9])
  const evidenceDigest = await digestBytes(canonicalBytes)
  await initial.commit({ canonicalBytes, digest: evidenceDigest })
  webLocks.crash()
  const recreated = createWebTerminalEvidenceSource(options)
  const resumed = await recreated.reserveTerminalEvidence(request)
  await expect(resumed.commit({ canonicalBytes, digest: evidenceDigest })).resolves.toMatchObject({
    durability: 'committed',
    byteLength: 1
  })
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 4 })])
  webLocks.crash()
})

test('uses a valid IndexedDB unsigned-32-bit getAll count for listing and staged retry', async () => {
  const { database, counts } = uint32GetAllDatabase()
  const source = createWebTerminalEvidenceSource({
    database,
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 2
  })

  await expect(source.listTerminalEvidence()).resolves.toEqual([])
  await expect(source.retryPendingTerminalEvidence()).resolves.toBeUndefined()
  expect(counts).toEqual([0xffffffff, 0xffffffff])
})

test.each(['staged', 'committed'])(
  'a recreated source releasing a %s row retains it until acknowledgement',
  async state => {
    const database = transactionalDatabase()
    const options = { database, locks: locks(), digest, digestBytes, maximumRetainedBytes: 3 }
    const request = {
      producer: 'scan',
      maximumBytes: 3,
      formatVersion: 1,
      scope: { managerId: 'm', operationId: `reattach-${state}`, incarnation: 'i' }
    }
    const original = createWebTerminalEvidenceSource(options)
    const initial = await original.reserveTerminalEvidence(request)
    const canonicalBytes = new Uint8Array([state === 'staged' ? 4 : 5])
    const receipt = await initial.commit({ canonicalBytes, digest: await digestBytes(canonicalBytes) })
    if (state === 'staged') {
      database.mutate('terminalEvidence', row => {
        row.state = 'staged'
      })
    }

    const recreated = createWebTerminalEvidenceSource(options)
    const reattached = await recreated.reserveTerminalEvidence(request)
    await reattached.release()

    expect(database.rows('terminalEvidence')).toEqual([expect.objectContaining({ ref: receipt.ref, state })])
    expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])
    if (state === 'staged') await recreated.retryPendingTerminalEvidence()
    await expect(recreated.readTerminalEvidence(receipt.ref, 3)).resolves.toMatchObject({
      kind: 'committed',
      canonicalBytes
    })
    await recreated.releaseTerminalEvidence(receipt.ref)
    expect(database.rows('terminalEvidence')).toEqual([])
    expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 0 })])
    await initial.release()
  }
)

test('reattaches a durable reserved row after a source restart without double charging quota', async () => {
  const database = transactionalDatabase()
  const webLocks = crashableLocks()
  const options = { database, locks: webLocks, digest, digestBytes, maximumRetainedBytes: 5 }
  const request = {
    producer: 'subscription',
    maximumBytes: 3,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'reserved-restart', incarnation: 'i' }
  }
  const initial = await createWebTerminalEvidenceSource(options).reserveTerminalEvidence(request)
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])
  webLocks.crash()
  const resumed = await createWebTerminalEvidenceSource(options).reserveTerminalEvidence(request)
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])
  const canonicalBytes = new Uint8Array([7])
  await expect(resumed.commit({ canonicalBytes, digest: await digestBytes(canonicalBytes) })).resolves.toMatchObject({
    durability: 'committed'
  })
  webLocks.crash()
  void initial
})

test('keeps scan and subscription reservations distinct for the same scope', async () => {
  const source = createWebTerminalEvidenceSource({
    database: transactionalDatabase(),
    locks: locks(),
    maximumRetainedBytes: 2,
    digest: async value => (value.includes('scan') ? 'c'.repeat(64) : 'd'.repeat(64)),
    digestBytes
  })
  const scope = { managerId: 'm', operationId: 'same', incarnation: 'i' }
  const scan = await source.reserveTerminalEvidence({ producer: 'scan', maximumBytes: 1, formatVersion: 1, scope })
  const subscription = await source.reserveTerminalEvidence({
    producer: 'subscription',
    maximumBytes: 1,
    formatVersion: 1,
    scope
  })
  expect(scan).not.toBe(subscription)
})

test('rejects a forged digest before durable terminal publication', async () => {
  const database = transactionalDatabase()
  const source = createWebTerminalEvidenceSource({
    database,
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 2
  })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'subscription',
    maximumBytes: 2,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'digest', incarnation: 'i' }
  })
  const canonicalBytes = new Uint8Array([1])
  await expect(reservation.commit({ canonicalBytes, digest: 'f'.repeat(64) })).rejects.toThrow(
    'web-terminal-evidence.digest'
  )
  const ref = database.rows('terminalEvidence')[0].ref
  await expect(source.readTerminalEvidence(ref, 2)).resolves.toEqual({ kind: 'unresolved' })
  const evidenceDigest = await digestBytes(canonicalBytes)
  await reservation.commit({ canonicalBytes, digest: evidenceDigest })
  database.mutate('terminalEvidence', row => {
    row.digest = '0'.repeat(64)
  })
  await expect(source.readTerminalEvidence(ref, 2)).rejects.toThrow('web-terminal-evidence.digest')
})

test('rejects a malformed persisted row from list, read, and retry rather than treating it as recovery evidence', async () => {
  const database = transactionalDatabase()
  const options = { database, locks: locks(), digest, digestBytes, maximumRetainedBytes: 2 }
  const source = createWebTerminalEvidenceSource(options)
  const reservation = await source.reserveTerminalEvidence({
    producer: 'scan',
    maximumBytes: 2,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'malformed-row', incarnation: 'i' }
  })
  const bytes = new Uint8Array([1])
  await reservation.commit({ canonicalBytes: bytes, digest: await digestBytes(bytes) })
  const ref = database.rows('terminalEvidence')[0].ref
  database.mutate('terminalEvidence', row => {
    row.formatVersion = 2
  })

  await expect(source.listTerminalEvidence()).rejects.toThrow('web-terminal-evidence.row')
  await expect(source.readTerminalEvidence(ref, 2)).rejects.toThrow('web-terminal-evidence.row')
  await expect(source.retryPendingTerminalEvidence()).rejects.toThrow('web-terminal-evidence.row')
})

test('atomically retires recovered committed evidence and returns its durable quota', async () => {
  const database = transactionalDatabase()
  const webLocks = crashableLocks()
  const source = createWebTerminalEvidenceSource({
    database,
    locks: webLocks,
    digest,
    digestBytes,
    maximumRetainedBytes: 3
  })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'scan',
    maximumBytes: 3,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'retire', incarnation: 'i' }
  })
  const canonicalBytes = new Uint8Array([3])
  await reservation.commit({ canonicalBytes, digest: await digestBytes(canonicalBytes) })
  const ref = database.rows('terminalEvidence')[0].ref
  await expect(source.releaseTerminalEvidence(ref)).rejects.toThrow('web-terminal-evidence.release-busy')
  webLocks.crash()
  await createWebTerminalEvidenceSource({
    database,
    locks: webLocks,
    digest,
    digestBytes,
    maximumRetainedBytes: 3
  }).releaseTerminalEvidence(ref)
  expect(database.rows('terminalEvidence')).toEqual([])
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 0 })])
})

test('uses one persisted aggregate quota across source instances and retains release ownership after an abort', async () => {
  const database = transactionalDatabase()
  const options = { database, locks: locks(), digest, digestBytes, maximumRetainedBytes: 4 }
  const first = createWebTerminalEvidenceSource(options)
  const second = createWebTerminalEvidenceSource(options)
  const firstReservation = await first.reserveTerminalEvidence({
    producer: 'scan',
    maximumBytes: 3,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'first', incarnation: 'i' }
  })
  await expect(
    second.reserveTerminalEvidence({
      producer: 'scan',
      maximumBytes: 2,
      formatVersion: 1,
      scope: { managerId: 'm', operationId: 'second', incarnation: 'i' }
    })
  ).rejects.toThrow('IndexedDB transaction aborted')
  expect(database.rows('terminalEvidenceState')).toEqual([
    expect.objectContaining({ maximumBytes: 4, reservedBytes: 3 })
  ])
  database.abortNext(new Error('injected-release-abort'))
  await expect(firstReservation.release()).rejects.toThrow('injected-release-abort')
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 3 })])
  await firstReservation.release()
  expect(database.rows('terminalEvidenceState')).toEqual([expect.objectContaining({ reservedBytes: 0 })])
  const secondReservation = await second.reserveTerminalEvidence({
    producer: 'scan',
    maximumBytes: 2,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'second', incarnation: 'i' }
  })
  await secondReservation.release()
})

test('keeps a failed commit durably reserved for an exact retry', async () => {
  const database = transactionalDatabase()
  const source = createWebTerminalEvidenceSource({
    database,
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 2
  })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'subscription',
    maximumBytes: 2,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'retry', incarnation: 'i' }
  })
  const canonicalBytes = new Uint8Array([2])
  const evidenceDigest = await digestBytes(canonicalBytes)
  database.abortNext(new Error('injected-commit-abort'))
  await expect(reservation.commit({ canonicalBytes, digest: evidenceDigest })).rejects.toThrow('injected-commit-abort')
  expect(database.rows('terminalEvidence')).toEqual([expect.objectContaining({ state: 'reserved' })])
  await expect(reservation.commit({ canonicalBytes, digest: evidenceDigest })).resolves.toMatchObject({
    durability: 'committed',
    digest: evidenceDigest
  })
  await reservation.release()
})

test('serializes cross-tab staged recovery and idempotent acknowledgement with the evidence lock', async () => {
  const database = transactionalDatabase()
  const webLocks = crashableLocks()
  const options = { database, locks: webLocks, digest, digestBytes, maximumRetainedBytes: 2 }
  const initial = createWebTerminalEvidenceSource(options)
  const reservation = await initial.reserveTerminalEvidence({
    producer: 'subscription',
    maximumBytes: 2,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'multi-tab', incarnation: 'i' }
  })
  const canonicalBytes = new Uint8Array([8])
  await reservation.commit({ canonicalBytes, digest: await digestBytes(canonicalBytes) })
  const ref = database.rows('terminalEvidence')[0].ref
  database.mutate('terminalEvidence', row => {
    row.state = 'staged'
  })
  webLocks.crash()

  const first = createWebTerminalEvidenceSource(options)
  const second = createWebTerminalEvidenceSource(options)
  const retries = await Promise.allSettled([
    first.retryPendingTerminalEvidence(),
    second.retryPendingTerminalEvidence()
  ])
  expect(retries.filter(result => result.status === 'fulfilled')).toHaveLength(1)
  expect(retries.filter(result => result.status === 'rejected')[0].reason).toMatchObject({
    normalized: { code: 'capability.unavailable' }
  })
  expect(database.rows('terminalEvidence')).toEqual([expect.objectContaining({ ref, state: 'committed' })])

  const acknowledgements = await Promise.allSettled([
    first.releaseTerminalEvidence(ref),
    second.releaseTerminalEvidence(ref)
  ])
  expect(acknowledgements.filter(result => result.status === 'fulfilled')).toHaveLength(1)
  expect(acknowledgements.filter(result => result.status === 'rejected')[0].reason).toMatchObject({
    normalized: { code: 'capability.unavailable' }
  })
  await expect(createWebTerminalEvidenceSource(options).releaseTerminalEvidence(ref)).resolves.toBeUndefined()
  expect(database.rows('terminalEvidence')).toEqual([])
})

test('hashes the exact bytes rather than decoded text', async () => {
  const source = createWebTerminalEvidenceSource({
    database: transactionalDatabase(),
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 4
  })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'scan',
    maximumBytes: 4,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'bytes', incarnation: 'i' }
  })
  const bytes = new Uint8Array([0xef, 0xbb, 0xbf, 0x61])
  await expect(reservation.commit({ canonicalBytes: bytes, digest: await digestBytes(bytes) })).resolves.toMatchObject({
    durability: 'committed'
  })
  await reservation.release()
})

test('coalesces concurrent identical commits into one durable receipt', async () => {
  const source = createWebTerminalEvidenceSource({
    database: transactionalDatabase(),
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 2
  })
  const reservation = await source.reserveTerminalEvidence({
    producer: 'subscription',
    maximumBytes: 2,
    formatVersion: 1,
    scope: { managerId: 'm', operationId: 'concurrent', incarnation: 'i' }
  })
  const bytes = new Uint8Array([5])
  const evidenceDigest = await digestBytes(bytes)
  await expect(
    Promise.all([
      reservation.commit({ canonicalBytes: bytes, digest: evidenceDigest }),
      reservation.commit({ canonicalBytes: bytes, digest: evidenceDigest })
    ])
  ).resolves.toEqual([
    expect.objectContaining({ durability: 'committed' }),
    expect.objectContaining({ durability: 'committed' })
  ])
  await reservation.release()
})

test('rejects invalid producer and format version before reserving quota', async () => {
  const source = createWebTerminalEvidenceSource({
    database: transactionalDatabase(),
    locks: locks(),
    digest,
    digestBytes,
    maximumRetainedBytes: 1
  })
  const scope = { managerId: 'm', operationId: 'invalid', incarnation: 'i' }
  await expect(
    source.reserveTerminalEvidence({ producer: 'other', maximumBytes: 1, formatVersion: 1, scope })
  ).rejects.toThrow('web-terminal-evidence.producer')
  await expect(
    source.reserveTerminalEvidence({ producer: 'scan', maximumBytes: 1, formatVersion: 2, scope })
  ).rejects.toThrow('web-terminal-evidence.format-version')
})
