const { webcrypto } = require('crypto')
const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const digest = async () => 'ab'.repeat(32)
const locks = () => ({ request: (_name, _options, callback) => Promise.resolve(callback({})) })
const declaration = {
  id: 'sample', maxBytes: 1048576, maxRecords: 2, collectionId: '1', sourceGeneration: 'source',
  memberEpochs: [{ member: '0', epoch: '1' }],
  context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c',
    databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4,
    members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d',
      serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' },
      delivery: 'notification', preference: null }] }
}
const declared = authority => ({ ...declaration, producerIncarnation: authority.createProducerIncarnation(webcrypto) })

test('declaration retains a current exact owner and blocks competing reservations', async () => {
  const database = recordingDatabase()
  const first = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'sample')
  expect(first.state).toBe('acquired')
  expect(await first.authority.declare(declared(first.authority))).toMatchObject({ state: 'prepared', ownerEpoch: '1', durability: null })
  expect(database.rows('authorityReservations')[0]).toMatchObject({ phase: 'provisional', generation: '1' })
  expect(first.authority.current()).toBe(true)
  const second = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'sample')
  expect(second).toMatchObject({ state: 'busy', lastCommitted: { state: 'prepared' } })
  await first.authority.release()
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('a worker probe and Web Lock contend on the same durable reservation', async () => {
  const database = recordingDatabase()
  const old = global.SharedWorker
  global.SharedWorker = class {
    constructor() {
      this.onerror = null
      this.port = { onmessage: null, onmessageerror: null, start() {}, close() {},
        postMessage(message) { queueMicrotask(() => this.onmessage({ data: message })) } }
    }
  }
  try {
    const worker = await createWebRecordingAuthority({ database, workerUrl: 'https://example.test/worker.js', digest, crypto: webcrypto }, 'sample')
    await worker.authority.declare(declared(worker.authority))
    const other = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'sample')
    expect(other.state).toBe('busy')
    expect(database.rows('readerReceipts')).toEqual([{ recordingId: ':ubm-authority-policy-v1', version: 2, protocol: 'ubm-authority-reservations/1' }])
    await worker.authority.release()
  } finally { global.SharedWorker = old }
})

test('cancellation during acquisition reconciles the exact provisional owner before releasing', async () => {
  const database = recordingDatabase(), controller = new AbortController()
  const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto, signal: controller.signal }, 'sample')
  const input = declared(result.authority)
  database.pauseNext()
  const acquiring = result.authority.declare(input)
  await database.requestsProcessed()
  expect(result.authority.current()).toBe(false)
  await expect(result.authority.declare(input)).rejects.toThrow()
  controller.abort()
  await expect(acquiring).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  const closing = result.authority.release()
  database.complete()
  await closing
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
})

test('a suppressed acquisition completion is reconciled by receipt and revoked', async () => {
  jest.useFakeTimers()
  try {
    const database = recordingDatabase()
    const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'sample')
    database.suppressNextCompletion()
    const acquiring = result.authority.declare(declared(result.authority))
    const outcome = acquiring.catch(error => error)
    await jest.advanceTimersByTimeAsync(5000)
    expect((await outcome).normalized.operation).toBe('web-recording.transaction-outcome-unknown')
    await result.authority.release()
    expect(database.rows('authorityReservations')[0].phase).toBe('released')
    expect(database.rows('recordings')[0].ownerState).toBe('prepared')
  } finally { jest.useRealTimers() }
})

test('release reconciles its own lost completion without starting another revoke', async () => {
  jest.useFakeTimers()
  try {
    const database = recordingDatabase()
    const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'sample')
    await result.authority.declare(declared(result.authority))
    database.suppressNextCompletion()
    const closing = result.authority.release()
    const outcome = closing.then(() => 'released')
    await jest.advanceTimersByTimeAsync(11000)
    expect(await outcome).toBe('released')
    expect(database.rows('authorityReservations')[0].phase).toBe('released')
  } finally { jest.useRealTimers() }
})

test('failed acquisition reconciliation can be retried without releasing the coordinator early', async () => {
  const database = recordingDatabase(), controller = new AbortController()
  const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto, signal: controller.signal }, 'sample')
  const input = declared(result.authority)
  database.pauseNext()
  const acquiring = result.authority.declare(input)
  await database.requestsProcessed()
  const transaction = database.transaction.bind(database)
  let fail = true
  database.transaction = (...args) => {
    const tx = transaction(...args)
    if (fail) { fail = false; tx.error = new Error('reconciliation unavailable'); tx.abort() }
    return tx
  }
  controller.abort()
  await expect(acquiring).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  const firstClose = result.authority.release()
  database.complete()
  await expect(firstClose).rejects.toThrow('reconciliation unavailable')
  expect(database.rows('authorityReservations')[0].phase).toBe('provisional')
  await result.authority.release()
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('a blocked reconciliation bounds close and leaves the exact task for retry', async () => {
  jest.useFakeTimers()
  try {
    const database = recordingDatabase(), controller = new AbortController()
    const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto, signal: controller.signal }, 'sample')
    database.pauseNext()
    const acquiring = result.authority.declare(declared(result.authority))
    await database.requestsProcessed()
    controller.abort()
    await expect(acquiring).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    const closing = result.authority.release().catch(error => error)
    await jest.advanceTimersByTimeAsync(5000)
    expect((await closing).normalized.operation).toBe('web-recording.transaction-outcome-unknown')
    database.complete()
    await result.authority.release()
    expect(database.rows('authorityReservations')[0].phase).toBe('released')
  } finally { jest.useRealTimers() }
})

test('the production bridge arms the exact reservation before opening the source', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const database = recordingDatabase()
  const { memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  await bridge.prepare()
  await bridge.open({ memberships: () => memberEpochs })
  expect(database.rows('recordings')[0].ownerState).toBe('live')
  expect(database.rows('authorityReservations')[0].phase).toBe('armed')
})

test('real supervisor drains and seals the durable journal before releasing the reservation', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const database = recordingDatabase()
  const { memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  const characteristic = {}
  const ingress = new WebNotificationIngress(() => {})
  const lease = { enable: jest.fn(async () => {}), release: jest.fn(async () => {}) }
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'x', characteristic, lease }], 8, 1024, () => true)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  expect((await supervisor.start()).state).toBe('active')
  ingress.admit(characteristic, new Uint8Array([7]))
  supervisor.stop()
  await supervisor.retry()
  expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
  expect(lease.release).toHaveBeenCalledTimes(1)
  expect(database.rows('records').map(row => row.record.kind)).toEqual(['value', 'terminal'])
  expect(database.rows('recordings')[0].ownerState).toBe('sealed')
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('cancellation after durable open but before installation closes the journal without enabling', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const database = recordingDatabase(), { memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  let opened, resume
  const reached = new Promise(resolve => { opened = resolve })
  const delayed = new Promise(resolve => { resume = resolve })
  const open = bridge.open
  bridge.open = async source => { await open(source); opened(); await delayed }
  const lease = { enable: jest.fn(async () => {}), release: jest.fn(async () => {}) }
  const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease }], 8, 1024, () => true)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge), abort = new AbortController()
  const start = supervisor.start(abort.signal)
  await reached
  abort.abort()
  await expect(start).rejects.toMatchObject({ name: 'AbortError' })
  resume()
  for (let i = 0; i < 30; i++) await Promise.resolve()
  await supervisor.retry()
  expect(lease.enable).not.toHaveBeenCalled()
  expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
  expect(database.rows('records').map(row => row.record.kind)).toEqual(['terminal'])
  expect(database.rows('recordings')[0].ownerState).toBe('sealed')
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('coordinator loss synchronously fences the real supervisor and retains the armed reservation', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const database = recordingDatabase(), { memberEpochs, ...input } = declaration
  let lose
  const manager = { request: (_name, _options, callback) => {
    callback({})
    return new Promise((_resolve, reject) => { lose = reject })
  } }
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: manager, digest, crypto: webcrypto }, input, webcrypto)
  const lease = { enable: jest.fn(async () => {}), release: jest.fn(async () => {}) }
  const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease }], 8, 1024, () => true)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  expect((await supervisor.start()).state).toBe('active')
  lose(new Error('lock lost'))
  await Promise.resolve()
  expect(supervisor.status().state).toBe('failed')
  await supervisor.retry()
  expect(lease.release).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')[0].phase).toBe('armed')
  expect(database.rows('recordings')[0].ownerState).toBe('live')
})

test('an aborted cleanup transaction can be retried with the same owner', async () => {
  const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
  const database = recordingDatabase()
  const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'sample')
  const input = declared(result.authority)
  const prepared = await result.authority.declare(input)
  const journal = createWebOrderedRecordingJournal(database, { ...input, ownerEpoch: prepared.ownerEpoch }, result.authority.reservationOwner())
  await journal.open('ef'.repeat(32))
  result.authority.noteOpenCommitted()
  database.abortNext(new Error('storage aborted'))
  await expect(result.authority.beginCleanup()).rejects.toThrow('storage aborted')
  expect(database.rows('authorityReservations')[0].phase).toBe('armed')
  await result.authority.beginCleanup()
  expect(database.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
})

test('a late member enable is compensated before durable authority release', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const database = recordingDatabase(), { memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  let resolveEnable, reportEnable
  const enabled = new Promise(resolve => { reportEnable = resolve })
  const lease = { enable: jest.fn(() => { reportEnable(); return new Promise(resolve => { resolveEnable = resolve }) }), release: jest.fn(async () => {}) }
  const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease }], 8, 1024, () => true)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  const start = supervisor.start()
  await enabled
  supervisor.stop()
  await supervisor.retry()
  expect(lease.release).not.toHaveBeenCalled()
  expect(database.rows('authorityReservations')[0].phase).not.toBe('released')
  resolveEnable()
  await start
  await supervisor.retry()
  expect(lease.release).toHaveBeenCalledTimes(1)
  expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('a committed delivery reports its receipt even if coordinator loss fences later work', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const database = recordingDatabase(), { memberEpochs, ...input } = declaration
  let lose
  const manager = { request: (_name, _options, callback) => {
    callback({})
    return new Promise((_resolve, reject) => { lose = reject })
  } }
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: manager, digest, crypto: webcrypto }, input, webcrypto)
  await bridge.prepare()
  await bridge.open({ memberships: () => memberEpochs })
  database.pauseNext()
  const commit = bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: false } })
  await database.requestsProcessed()
  lose(new Error('lost'))
  await Promise.resolve()
  database.complete()
  await expect(commit).resolves.toBe(1)
  expect(bridge.canAcknowledge()).toBe(false)
  expect(database.rows('records')).toHaveLength(1)
})

test('stop records cleanup-pending even when a subsequent journal delivery fails', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const database = recordingDatabase(), { memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  bridge.commit = async () => { throw new Error('delivery unavailable') }
  const lease = { enable: async () => {}, release: jest.fn(async () => {}) }
  const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease }], 8, 1024, () => true)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  await supervisor.start()
  supervisor.stop()
  await supervisor.retry()
  expect(database.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
  expect(lease.release).toHaveBeenCalledTimes(1)
  expect(supervisor.status().state).toBe('failed')
})

test('a lost seal completion is retried behind the original transaction and exact receipt', async () => {
  jest.useFakeTimers()
  try {
    const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
    const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
    const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
    const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
    const database = recordingDatabase(), { memberEpochs, ...input } = declaration
    const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
    const seal = bridge.seal
    let first = true
    bridge.seal = () => { if (first) { first = false; database.suppressNextCompletion() }; return seal() }
    const lease = { enable: async () => {}, release: async () => {} }
    const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease }], 8, 1024, () => true)
    const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
    await supervisor.start()
    supervisor.stop()
    await jest.advanceTimersByTimeAsync(30001)
    expect(database.rows('recordings')[0].ownerState).toBe('sealed')
    await supervisor.retry()
    expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
    expect(database.rows('authorityReservations')[0].phase).toBe('released')
  } finally { jest.useRealTimers() }
})

test('failed member cleanup keeps the unsealed authority reserved until retry succeeds', async () => {
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const database = recordingDatabase(), { memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  let attempts = 0
  const lease = { enable: async () => {}, release: jest.fn(async () => { if (++attempts === 1) throw new Error('release failed') }) }
  const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease }], 8, 1024, () => true)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  await supervisor.start()
  supervisor.stop()
  await supervisor.retry()
  expect(supervisor.status().state).toBe('failed')
  expect(database.rows('recordings')[0].ownerState).toBe('live')
  expect(database.rows('records').some(row => row.record.kind === 'terminal')).toBe(false)
  expect(database.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
  await supervisor.retry()
  expect(lease.release).toHaveBeenCalledTimes(2)
  expect(supervisor.status().state).toBe('finished')
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('a lost delivery commit completion is reconciled before the source is acknowledged', async () => {
  jest.useFakeTimers()
  try {
    const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
    const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
    const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
    const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
    const database = recordingDatabase(), { memberEpochs, ...input } = declaration
    const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
    const commit = bridge.commit
    let first = true
    bridge.commit = delivery => { if (first) { first = false; database.suppressNextCompletion() }; return commit(delivery) }
    const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease: { enable: async () => {}, release: async () => {} } }], 8, 1024, () => true)
    const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
    await supervisor.start()
    supervisor.stop()
    await jest.advanceTimersByTimeAsync(30001)
    await supervisor.retry()
    expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
    expect(database.rows('records')).toHaveLength(1)
    expect(database.rows('authorityReservations')[0].phase).toBe('released')
  } finally { jest.useRealTimers() }
})

test('a lost retirement completion is reconciled before sealing', async () => {
  jest.useFakeTimers()
  try {
    const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
    const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
    const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
    const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
    const database = recordingDatabase(), { memberEpochs, ...input } = declaration
    const bridge = createWebAuthorityRecordingJournalBridge({ database, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
    const retire = bridge.retire
    let first = true
    bridge.retire = id => { if (first) { first = false; database.suppressNextCompletion() }; return retire(id) }
    const owner = new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease: { enable: async () => {}, release: async () => {} } }], 8, 1024, () => true)
    const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
    await supervisor.start()
    supervisor.stop()
    await jest.advanceTimersByTimeAsync(30001)
    await supervisor.retry()
    expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
    expect(database.rows('recordings')[0].pendingCount).toBe(0)
    expect(database.rows('authorityReservations')[0].phase).toBe('released')
  } finally { jest.useRealTimers() }
})

test('revalidation fences admission until the exact reservation and graph are checked', async () => {
  const database = recordingDatabase()
  const losses = []
  const { authority } = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto, onLoss: error => losses.push(error) }, 'sample')
  await authority.declare(declared(authority))
  const originalOwner = authority.reservationOwner()
  database.pauseNext()
  const check = authority.revalidate(() => true)
  expect(authority.current()).toBe(false)
  await database.requestsProcessed()
  const closing = authority.release()
  database.complete()
  await check
  await closing
  expect(database.rows('authorityReservations')[0]).toMatchObject({ ...originalOwner, phase: 'released' })
  expect(losses).toEqual([])
})

test.each(['token', 'graph'])('revalidation fences a changed %s and retains durable evidence', async change => {
  const database = recordingDatabase()
  const losses = []
  const { authority } = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto, onLoss: error => losses.push(error) }, 'sample')
  await authority.declare(declared(authority))
  const owner = authority.reservationOwner()
  if (change === 'token') database.mutate('authorityReservations', row => { row.ownerToken = 'ab'.repeat(32) })
  const before = database.rows('recordings')
  await expect(authority.revalidate(() => change !== 'graph')).rejects.toThrow()
  expect(authority.current()).toBe(false)
  expect(losses).toHaveLength(1)
  expect(database.rows('recordings')).toEqual(before)
  if (change === 'token') database.mutate('authorityReservations', row => { row.ownerToken = owner.ownerToken })
  await authority.release()
})

test('revalidation rejects an armed reservation when the local owner has not observed its open', async () => {
  const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
  const database = recordingDatabase()
  const losses = []
  const { authority } = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto, onLoss: error => losses.push(error) }, 'sample')
  await authority.declare(declared(authority))
  const owner = authority.reservationOwner()
  const original = database.rows('authorityReservations')[0]
  await createWebAuthorityReservations(database).arm(owner, 'ef'.repeat(32))
  await expect(authority.revalidate(() => true)).rejects.toThrow()
  expect(authority.current()).toBe(false)
  expect(losses).toHaveLength(1)
  expect(database.rows('recordings')[0]).toMatchObject({ ownerEpoch: owner.ownerEpoch, ownerState: 'live' })
  database.mutate('authorityReservations', row => Object.assign(row, original))
  database.mutate('recordings', row => { row.ownerState = 'prepared' })
  await authority.release()
})
