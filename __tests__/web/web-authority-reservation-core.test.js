const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const declaration = {
  id: 'sample', maxBytes: 1048576, maxRecords: 2, producerIncarnation: 'producer',
  collectionId: '1', sourceGeneration: 'source', memberEpochs: [{ member: '0', epoch: '1' }],
  context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c',
    databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4,
    members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d',
      serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' },
      delivery: 'notification', preference: null }] }
}
const token = n => n.repeat(64)
const request = (n, coordinator, expectedGeneration = null) => ({ declaration, ownerToken: token(n), operationId: token(n), coordinator, expectedGeneration })
const lock = { kind: 'locks', name: 'lock' }
const worker = { kind: 'worker', url: 'https://example.com/worker.js', name: 'worker', type: 'module' }

test('independent lock and worker instances serialize admission on the recording', async () => {
  const db = recordingDatabase()
  const a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const [first, second] = await Promise.all([a.acquire(request('a', lock)), b.acquire(request('b', worker))])
  expect(first.state).toBe('acquired')
  expect(second.state).toBe('busy')
  expect(db.rows('recordings')).toHaveLength(1)
  expect(db.rows('authorityReservations')[0]).toMatchObject({ phase: 'provisional', generation: '1' })
})

test('revocation wins over late arm; armed wins over later revocation', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  const revoked = await b.revokeProvisional(first.owner, token('c'))
  expect(revoked.state).toBe('released')
  const next = await b.acquire(request('b', worker, '1'))
  expect(next.owner.generation).toBe('2')
  await expect(a.arm(first.owner, token('d'))).rejects.toThrow()
  await b.arm(next.owner, token('e'))
  expect(db.rows('recordings')[0]).toMatchObject({ ownerEpoch: '2', ownerState: 'live' })
  await expect(a.revokeProvisional(next.owner, token('f'))).rejects.toThrow()
  expect((await a.acquire(request('c', lock))).state).toBe('busy')
})

test('arm and revocation racing have exactly one winning transaction', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  const outcomes = await Promise.allSettled([a.arm(first.owner, token('b')), b.revokeProvisional(first.owner, token('c'))])
  expect(outcomes.filter(outcome => outcome.status === 'fulfilled')).toHaveLength(1)
  expect(db.rows('authorityReservations')[0].phase).toBe('armed')
})

test('reconciles acquisition and committed arm by exact operation identity', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  expect(await b.reconcileAcquire('sample', token('a'), token('a'))).toEqual({ outcome: 'committed', owner: first.owner })
  await a.arm(first.owner, token('b'))
  expect(await b.reconcile(first.owner, token('b'))).toMatchObject({ outcome: 'committed', result: { state: 'armed' } })
  expect(await b.reconcile(first.owner, token('c'))).toEqual({ outcome: 'unknown' })
  expect(await b.reconcileAcquire('sample', token('a'), token('a'))).toEqual({ outcome: 'unknown' })
})

test('corrupt declaration is never armed and transaction aborts preserve the journal', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  db.mutate('recordings', row => { row.maxRecords = 999 })
  await expect(a.arm(first.owner, token('b'))).rejects.toThrow()
  expect(db.rows('recordings')[0].ownerState).toBe('prepared')
  expect(db.rows('authorityReservations')[0].phase).toBe('provisional')
})

test('retry of the same acquisition returns its committed owner without incrementing the epoch', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  expect(await a.acquire(request('a', lock))).toEqual(first)
  expect(db.rows('recordings')[0].ownerEpoch).toBe('1')
})

test('aborted quota failure leaves neither reservation nor declaration', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  db.abortNext(new Error('QuotaExceededError'))
  await expect(a.acquire(request('a', lock))).rejects.toThrow('QuotaExceededError')
  expect(db.rows('recordings')).toEqual([])
  expect(db.rows('authorityReservations')).toEqual([])
})

test('generation exhaustion preserves released reservation and journal', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  await a.revokeProvisional(first.owner, token('b'))
  db.mutate('authorityReservations', row => { row.generation = '18446744073709551615' })
  await expect(a.acquire(request('c', worker, '18446744073709551615'))).rejects.toThrow()
  expect(db.rows('recordings')[0].ownerEpoch).toBe('1')
  expect(db.rows('authorityReservations')[0].phase).toBe('released')
})

test('armed owner stays busy until cleanup and a terminal journal are both recorded', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  await a.arm(first.owner, token('b'))
  await expect(a.release(first.owner, token('c'))).rejects.toThrow()
  await a.cleanupPending(first.owner, token('d'))
  await expect(a.release(first.owner, token('c'))).rejects.toThrow()
  expect((await b.acquire(request('e', worker))).state).toBe('busy')
  db.mutate('recordings', row => { row.ownerState = 'sealed' })
  expect(await a.release(first.owner, token('c'))).toMatchObject({ state: 'released' })
  expect(await a.release(first.owner, token('c'))).toMatchObject({ state: 'released' })
})

test('a fresh overlapping transaction reconciles a committed arm whose completion was not delivered', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  db.suppressNextCompletion()
  void a.arm(first.owner, token('b'))
  const outcome = await b.reconcile(first.owner, token('b'))
  expect(outcome).toMatchObject({ outcome: 'committed', result: { state: 'armed' } })
  expect((await b.acquire(request('c', worker))).state).toBe('busy')
})

test('owner identities use all 256 bits from the supplied cryptographic source', () => {
  const { createWebAuthorityToken } = require('../../src/web/web-authority-reservation')
  const source = { getRandomValues: bytes => { bytes.fill(0x91); return bytes } }
  expect(createWebAuthorityToken(source)).toBe('91'.repeat(32))
})

test('late retry cannot resurrect a revoked owner token as a new generation', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  await a.revokeProvisional(first.owner, token('b'))
  await expect(a.acquire(request('a', lock))).rejects.toThrow()
  expect(db.rows('authorityReservations')[0]).toMatchObject({ phase: 'released', generation: '1' })
})

test('recovery can observe and revoke a provisional generation without the owner token', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db), b = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  expect(await b.status('sample')).toMatchObject({ phase: 'provisional', generation: '1' })
  expect(await b.revokeObservedProvisional('sample', '1', token('b'))).toMatchObject({ state: 'released' })
  await expect(a.arm(first.owner, token('c'))).rejects.toThrow()
  expect((await b.acquire(request('d', worker, '1'))).state).toBe('acquired')
})

test('malformed journal accounting blocks an arm', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  const first = await a.acquire(request('a', lock))
  db.mutate('recordings', row => { row.nextOrdinal = -1 })
  await expect(a.arm(first.owner, token('b'))).rejects.toThrow()
  expect(db.rows('authorityReservations')[0].phase).toBe('provisional')
})

test('acquire snapshots its declaration before asynchronous transaction callbacks', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  const mutable = structuredClone(declaration)
  const pending = a.acquire({ ...request('a', lock), declaration: mutable })
  mutable.maxRecords = 999
  mutable.memberEpochs[0].epoch = '2'
  await pending
  expect(db.rows('recordings')[0]).toMatchObject({ maxRecords: 2, memberEpochs: [{ member: '0', epoch: '1' }] })
})

test('an existing legacy coordinator policy prevents admission', async () => {
  const db = recordingDatabase(), a = createWebAuthorityReservations(db)
  db.seed('readerReceipts', { recordingId: ':ubm-authority-policy-v1', version: 1, coordinator: 'locks' })
  await expect(a.acquire(request('a', lock))).rejects.toThrow()
  expect(db.rows('recordings')).toEqual([])
  expect(db.rows('authorityReservations')).toEqual([])
})

test('the old authority refuses the reservation protocol sentinel', async () => {
  const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
  const db = recordingDatabase()
  await expect(createWebRecordingAuthority({ database: db, locks: { request: jest.fn() }, digest: async () => 'ab'.repeat(32) }, 'sample')).rejects.toThrow()
  expect(db.rows('recordings')).toEqual([])
})
