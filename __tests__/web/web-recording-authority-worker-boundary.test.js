const { webcrypto } = require('crypto')
const { installWebRecordingAuthorityWorker } = require('../../src/web/web-recording-authority-worker')
const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { acquireWebRecordingWorkerLock, createWebRecordingSharedWorkerFactory } = require('../../src/web/web-recording-authority-worker-client')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const url = 'https://example.test/worker.js'
const digest = async () => '12'.repeat(32)
const locks = () => ({ request: (_name, _options, callback) => Promise.resolve(callback({})) })
const options = database => ({ database, workerUrl: url, digest, crypto: webcrypto })
const declaration = authority => ({
  id: 'id', maxBytes: 1048576, maxRecords: 2, collectionId: '1', sourceGeneration: 's',
  producerIncarnation: authority.createProducerIncarnation(webcrypto),
  memberEpochs: [{ member: '0', epoch: '1' }],
  context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c',
    databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4,
    members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d',
      serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' },
      delivery: 'notification', preference: null }] }
})

// The real worker protocol runs on paired deterministic ports; delivery queued before
// close remains deliverable, like a posted MessagePort message.
function workerHost() {
  const scope = { onconnect: null }, clients = [], calls = []
  installWebRecordingAuthorityWorker(scope)
  class Worker {
    constructor(workerUrl, workerOptions) {
      calls.push([workerUrl, workerOptions])
      const client = { onmessage: null, onmessageerror: null, onclose: null, start: jest.fn(), close: jest.fn(),
        postMessage: jest.fn(data => queueMicrotask(() => server.onmessage?.({ data }))) }
      const server = { onmessage: null, onmessageerror: null, onclose: null, start: jest.fn(), close: jest.fn(),
        postMessage: jest.fn(data => queueMicrotask(() => client.onmessage?.({ data }))) }
      this.port = client
      this.onerror = null
      clients.push(this)
      scope.onconnect({ ports: [server] })
    }
  }
  return { Worker, clients, calls }
}
let oldWorker
beforeEach(() => { oldWorker = global.SharedWorker })
afterEach(() => { global.SharedWorker = oldWorker; jest.useRealTimers() })

function manualWorker() {
  const clients = []
  global.SharedWorker = class {
    constructor() {
      this.onerror = null
      this.port = { onmessage: null, onmessageerror: null, onclose: null, start: jest.fn(), close: jest.fn(), postMessage: jest.fn() }
      clients.push(this)
    }
  }
  return clients
}
const reply = port => port.onmessage({ data: port.postMessage.mock.calls[0][0] })

// A probe is compatibility evidence, while the transactional declaration is the grant.
test('real worker accepts HELLO, pins its fixed identity, and closes the probe port', async () => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase()
  const result = await createWebRecordingAuthority({ ...options(database), workerUrl: 'https://example.test/a/../worker.js' }, 'id')
  expect(result.state).toBe('acquired')
  expect(host.calls).toEqual([[url, { name: 'ubm-ordered-recording-v2', type: 'module' }]])
  expect(host.clients[0].port.postMessage).toHaveBeenCalledWith(expect.objectContaining({ type: 'hello', version: 2 }))
  expect(host.clients[0].port.close).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')).toEqual([])
  await result.authority.release()
})

test.each([{ type: 'hello', version: 1, requestId: '1' }, { type: 'hello', version: 2, requestId: 'stale' }, { state: 'acquired' }, null])('incompatible worker reply %j refuses authority and closes the port', async data => {
  const clients = manualWorker(), database = recordingDatabase()
  const pending = createWebRecordingAuthority(options(database), 'id')
  clients[0].port.onmessage({ data })
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(clients[0].port.close).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')).toEqual([])
})

test('real worker refuses old acquire messages and incompatible versions', () => {
  const scope = { onconnect: null }; installWebRecordingAuthorityWorker(scope)
  for (const data of [{ type: 'acquire', name: 'id' }, { type: 'hello', version: 1, requestId: '1' }]) {
    const port = { onmessage: null, onmessageerror: null, start: jest.fn(), close: jest.fn(), postMessage: jest.fn() }
    scope.onconnect({ ports: [port] }); port.onmessage({ data })
    expect(port.postMessage).not.toHaveBeenCalled(); expect(port.close).toHaveBeenCalledTimes(1)
  }
})

test('legacy worker lock helper fails closed without constructing a worker', async () => {
  const factory = jest.fn()
  await expect(acquireWebRecordingWorkerLock(factory, digest, 'id')).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(factory).not.toHaveBeenCalled()
})

test('the legacy factory still pins worker name and module type', () => {
  const calls = []
  class Worker { constructor(...args) { calls.push(args) } }
  const factory = createWebRecordingSharedWorkerFactory(Worker, url)
  factory(); factory()
  expect(calls).toEqual(Array(2).fill([url, { name: 'ubm-ordered-recording-v2', type: 'module' }]))
  expect(factory.coordinatorId).toBe(url)
  expect(() => createWebRecordingSharedWorkerFactory(Worker, './relative.js')).toThrow()
})

test('absence, invalid URL and construction failure do not create reservations', async () => {
  const database = recordingDatabase()
  await expect(createWebRecordingAuthority({ database, digest, crypto: webcrypto }, 'id')).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  await expect(createWebRecordingAuthority({ ...options(database), workerUrl: './worker.js' }, 'id')).rejects.toMatchObject({ normalized: { code: 'argument.invalid' } })
  global.SharedWorker = class { constructor() { throw new Error('denied') } }
  await expect(createWebRecordingAuthority(options(database), 'id')).rejects.toMatchObject({ normalized: { code: 'capability.unsupported', platform: { domain: 'SharedWorker' } } })
  expect(database.rows('authorityReservations')).toEqual([])
  const result = await createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'id')
  await result.authority.release()
})

test.each(['error', 'messageerror', 'close', 'start', 'postMessage'])('worker %s failure refuses acquisition and closes its port', async failure => {
  const clients = manualWorker(), database = recordingDatabase()
  if (failure === 'start' || failure === 'postMessage') {
    const Base = global.SharedWorker
    global.SharedWorker = class extends Base { constructor(...args) { super(...args); this.port[failure] = jest.fn(() => { throw new Error('denied') }) } }
  }
  const pending = createWebRecordingAuthority(options(database), 'id')
  if (failure === 'error') clients[0].onerror(new Error('denied'))
  if (failure === 'messageerror') clients[0].port.onmessageerror()
  if (failure === 'close') clients[0].port.onclose()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(clients[0].port.close).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')).toEqual([])
})

test.each(['construct', 'error', 'start', 'postMessage'])('%s startup failure preserves a safe browser error classification', async failure => {
  const clients = manualWorker(), database = recordingDatabase(), Base = global.SharedWorker
  const denied = new DOMException('private worker URL and browser detail', 'SecurityError')
  global.SharedWorker = class extends Base {
    constructor(...args) {
      if (failure === 'construct') throw denied
      super(...args)
      if (failure === 'start' || failure === 'postMessage') this.port[failure] = jest.fn(() => { throw denied })
    }
  }
  const pending = createWebRecordingAuthority(options(database), 'id')
  if (failure === 'error') clients[0].onerror({ error: denied, message: 'private event detail', filename: url })
  let caught
  try { await pending } catch (error) { caught = error }
  expect(caught).toMatchObject({ normalized: { code: 'capability.unsupported', platform: { domain: 'SharedWorker', code: 'SecurityError' } } })
  expect(JSON.stringify(caught.normalized)).not.toMatch(/private|example\.test/)
  if (failure !== 'construct') expect(clients[0].port.close).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')).toEqual([])
})

test('worker error without a safe browser error name retains a generic SharedWorker classification', async () => {
  const clients = manualWorker(), database = recordingDatabase()
  const pending = createWebRecordingAuthority(options(database), 'id')
  clients[0].onerror({ message: 'secret detail', error: { name: 'secret detail' } })
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unsupported', platform: { domain: 'SharedWorker', code: 'unknown' } } })
})

test('silent probes time out, close their ports, and permit subsequent retry', async () => {
  jest.useFakeTimers()
  try {
    const clients = manualWorker(), database = recordingDatabase()
    for (let i = 0; i < 3; i++) {
      const pending = createWebRecordingAuthority(options(database), 'id')
      const rejected = expect(pending).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
      await jest.advanceTimersByTimeAsync(5000); await rejected
      expect(clients[i].port.close).toHaveBeenCalledTimes(1)
    }
    const retry = createWebRecordingAuthority(options(database), 'id'); reply(clients[3].port)
    const result = await retry; expect(result.state).toBe('acquired'); await result.authority.release()
  } finally { jest.useRealTimers() }
})

test('cancellation closes the probe; its stale callback cannot satisfy a later request', async () => {
  const clients = manualWorker(), database = recordingDatabase(), controller = new AbortController()
  const pending = createWebRecordingAuthority({ ...options(database), signal: controller.signal }, 'id')
  const stale = clients[0].port.onmessage, oldRequest = clients[0].port.postMessage.mock.calls[0][0]
  controller.abort()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  expect(clients[0].port.close).toHaveBeenCalledTimes(1)
  const retry = createWebRecordingAuthority(options(database), 'id')
  expect(clients[1].port.postMessage.mock.calls[0][0].requestId).not.toBe(oldRequest.requestId)
  stale({ data: oldRequest })
  clients[1].port.onmessage({ data: oldRequest })
  await expect(retry).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(clients[1].port.close).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')).toEqual([])
})

test('abort after HELLO promptly settles paused reservation status and allows a later retry', async () => {
  jest.useFakeTimers()
  try {
  const clients = manualWorker(), database = recordingDatabase(), controller = new AbortController()
  database.pauseNext()
  const pending = createWebRecordingAuthority({ ...options(database), signal: controller.signal }, 'id')
  reply(clients[0].port)
  await database.requestsProcessed()
  const rejected = expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  controller.abort()
  const bounded = rejected.then(() => 'aborted')
  await jest.advanceTimersByTimeAsync(100)
  expect(await bounded).toBe('aborted')
  expect(clients[0].port.close).toHaveBeenCalledTimes(1)
  expect(database.rows('authorityReservations')).toEqual([])
  database.complete()
  const retry = createWebRecordingAuthority(options(database), 'id'); reply(clients[1].port)
  const later = retry
  await jest.advanceTimersByTimeAsync(100)
  const result = await later
  expect(result.state).toBe('acquired')
  await result.authority.declare(declaration(result.authority))
  expect(database.rows('authorityReservations')[0].phase).toBe('provisional')
  await result.authority.release()
  } finally { jest.useRealTimers() }
})

test('concurrent compatible probes can create handles but only one declaration reserves authority', async () => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase(), opts = options(database)
  const [a, b] = await Promise.all([createWebRecordingAuthority(opts, 'id'), createWebRecordingAuthority(opts, 'id')])
  const results = await Promise.allSettled([a.authority.declare(declaration(a.authority)), b.authority.declare(declaration(b.authority))])
  expect(results.filter(x => x.status === 'fulfilled')).toHaveLength(1)
  expect(results.filter(x => x.status === 'rejected')[0].reason).toMatchObject({ normalized: { code: 'capability.unavailable' } })
  expect(database.rows('authorityReservations')).toHaveLength(1)
  expect(database.rows('authorityReservations')[0].phase).toBe('provisional')
  await a.authority.release(); await b.authority.release()
})

test.each(['worker-first', 'locks-first'])('concurrent %s declarations share one durable grant', async order => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase()
  const [worker, lock] = await Promise.all([
    createWebRecordingAuthority(options(database), 'id'),
    createWebRecordingAuthority({ database, locks: locks(), digest, crypto: webcrypto }, 'id')
  ])
  expect(worker.state).toBe('acquired'); expect(lock.state).toBe('acquired')
  const authorities = order === 'worker-first' ? [worker.authority, lock.authority] : [lock.authority, worker.authority]
  const inputs = authorities.map(declaration)
  const outcomes = await Promise.allSettled(authorities.map((authority, index) => authority.declare(inputs[index])))
  expect(outcomes.filter(outcome => outcome.status === 'fulfilled')).toHaveLength(1)
  expect(outcomes.filter(outcome => outcome.status === 'rejected')[0].reason).toMatchObject({ normalized: { code: 'capability.unavailable' } })
  expect(database.rows('authorityReservations')).toHaveLength(1)
  expect(database.rows('authorityReservations')[0]).toMatchObject({ phase: 'provisional', generation: '1', coordinator: { kind: order === 'worker-first' ? 'worker' : 'locks' } })
  expect(database.rows('recordings')).toHaveLength(1)
  await Promise.all(authorities.map(authority => authority.release()))
})

test.each(['worker', 'locks'])('a %s reservation blocks the other coordinator until cleanup and permits handoff', async choice => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase(), worker = options(database), lock = { database, locks: locks(), digest, crypto: webcrypto }
  const first = (await createWebRecordingAuthority(choice === 'worker' ? worker : lock, 'id')).authority
  await first.declare(declaration(first))
  const other = choice === 'worker' ? lock : worker
  expect(await createWebRecordingAuthority(other, 'id')).toMatchObject({ state: 'busy', lastCommitted: { state: 'prepared' } })
  let finish
  const closing = first.close(() => new Promise(resolve => { finish = resolve }))
  expect(await createWebRecordingAuthority(other, 'id')).toMatchObject({ state: 'busy' })
  finish(); await closing
  const next = (await createWebRecordingAuthority(other, 'id')).authority
  expect(await next.declare(declaration(next))).toMatchObject({ ownerEpoch: '2' })
  expect(database.rows('authorityReservations')[0]).toMatchObject({ phase: 'provisional', generation: '2', coordinator: { kind: choice === 'worker' ? 'locks' : 'worker' } })
  await next.release()
})

test('different worker URLs and worker restart cannot bypass a durable reservation', async () => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase(), opts = options(database)
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  await first.declare(declaration(first))
  const restarted = workerHost(); global.SharedWorker = restarted.Worker
  expect(await createWebRecordingAuthority({ ...opts, workerUrl: 'https://example.test/other.js' }, 'id')).toMatchObject({ state: 'busy' })
  expect(database.rows('authorityReservations')[0].coordinator).toMatchObject({ kind: 'worker', url })
  await first.release()
  const next = (await createWebRecordingAuthority({ ...opts, workerUrl: 'https://example.test/other.js' }, 'id')).authority
  await next.declare(declaration(next))
  expect(database.rows('authorityReservations')[0]).toMatchObject({ generation: '2', coordinator: { url: 'https://example.test/other.js' } })
  await next.release()
})

test('a silent durable owner is not expired by the worker handshake timeout', async () => {
  jest.useFakeTimers()
  try {
    const clients = manualWorker(), database = recordingDatabase(), opts = options(database)
    const acquiring = createWebRecordingAuthority(opts, 'id'); reply(clients[0].port)
    const first = (await acquiring).authority
    await first.declare(declaration(first))
    await jest.advanceTimersByTimeAsync(60000)
    const competing = createWebRecordingAuthority(opts, 'id'); reply(clients[1].port)
    expect(await competing).toMatchObject({ state: 'busy', lastCommitted: { state: 'prepared' } })
    expect(first.current()).toBe(true)
    await first.release()
  } finally { jest.useRealTimers() }
})

test('a closed probe and stale port callbacks do not fence the durable owner or release it', async () => {
  const clients = manualWorker(), database = recordingDatabase(), onLoss = jest.fn()
  const pending = createWebRecordingAuthority({ ...options(database), onLoss }, 'id')
  const staleClose = clients[0].port.onclose
  reply(clients[0].port)
  const first = (await pending).authority
  await first.declare(declaration(first))
  staleClose()
  expect(first.current()).toBe(true); expect(onLoss).not.toHaveBeenCalled()
  const contender = createWebRecordingAuthority(options(database), 'id'); reply(clients[1].port)
  expect(await contender).toMatchObject({ state: 'busy', lastCommitted: { state: 'prepared' } })
  await first.release()
})

test('a stale probe close preserves the live journal and does not admit a replacement', async () => {
  const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
  const clients = manualWorker(), database = recordingDatabase()
  const pending = createWebRecordingAuthority(options(database), 'id')
  const staleClose = clients[0].port.onclose
  reply(clients[0].port)
  const first = (await pending).authority, input = declaration(first)
  const prepared = await first.declare(input)
  const journal = createWebOrderedRecordingJournal(database, { ...input, ownerEpoch: prepared.ownerEpoch }, first.reservationOwner())
  await journal.open('c'.repeat(64)); first.noteOpenCommitted()
  staleClose()
  const contender = createWebRecordingAuthority(options(database), 'id'); reply(clients[1].port)
  expect(await contender).toMatchObject({ state: 'busy', lastCommitted: { state: 'live' } })
  await expect(journal.commit({ id: '1', record: { kind: 'value', ordinal: '1', member: '0',
    membership: { member: '0', epoch: '1' }, bytes: new Uint8Array([1]), delivery: 'notification',
    provenance: 'read-or-notification' } })).resolves.toBeDefined()
  expect(database.rows('records')).toHaveLength(1)
  expect(database.rows('authorityReservations')[0].phase).toBe('armed')
  expect(database.rows('recordings')[0].ownerState).toBe('live')
})

test('restarted worker keeps an armed journal busy through failed cleanup and retry until terminal release', async () => {
  const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase(), opts = options(database)
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  const input = declaration(first), prepared = await first.declare(input)
  const journal = createWebOrderedRecordingJournal(database, { ...input, ownerEpoch: prepared.ownerEpoch }, first.reservationOwner())
  await journal.open('c'.repeat(64)); first.noteOpenCommitted()
  expect(database.rows('authorityReservations')[0].phase).toBe('armed')
  const restarted = workerHost(); global.SharedWorker = restarted.Worker
  expect(await createWebRecordingAuthority(opts, 'id')).toMatchObject({ state: 'busy', lastCommitted: { state: 'live' } })
  await first.beginCleanup()
  await expect(first.close(async () => { throw new Error('producer cleanup failed') })).rejects.toThrow('producer cleanup failed')
  expect(database.rows('authorityReservations')[0].phase).toBe('cleanup-pending')
  expect(database.rows('recordings')[0].ownerState).toBe('live')
  expect(await createWebRecordingAuthority(opts, 'id')).toMatchObject({ state: 'busy', lastCommitted: { state: 'live' } })
  let finishCleanup, cleanupStarted
  const started = new Promise(resolve => { cleanupStarted = resolve })
  const closing = first.close(async () => {
    cleanupStarted()
    await new Promise(resolve => { finishCleanup = resolve })
    await journal.commit({ id: '1', outcome: { version: 1, state: 'finished', error: null },
      status: { recordingId: 'id', phase: 'stopped', accepting: false, records: 1, bytes: 1, lostRecords: 0,
        maxBytes: input.maxBytes, maxRecords: input.maxRecords, encrypted: false,
        orderedOwner: { version: 1, producerIncarnation: input.producerIncarnation, collectionId: '1',
          sourceGeneration: { kind: 'opaque', value: 's' }, state: 'sealed', uncertainTail: null },
        runtimeFailure: null, collectionFailure: null },
      record: { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false, rejectedValues: null,
        rejectedLoss: null, sourceRejection: null, sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0' } })
    await journal.retire('1')
    await journal.seal()
  })
  await started
  expect(await createWebRecordingAuthority(opts, 'id')).toMatchObject({ state: 'busy', lastCommitted: { state: 'live' } })
  finishCleanup(); await closing
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
  expect(database.rows('recordings')[0].ownerState).toBe('sealed')
  const next = await createWebRecordingAuthority(opts, 'id')
  expect(next.state).toBe('acquired')
  await next.authority.release()
})

test('failed cleanup retains authority until retry completes', async () => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase(), opts = options(database)
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  await first.declare(declaration(first))
  await expect(first.close(async () => { throw new Error('cleanup failed') })).rejects.toThrow('cleanup failed')
  expect(database.rows('authorityReservations')[0].phase).toBe('provisional')
  expect(await createWebRecordingAuthority(opts, 'id')).toMatchObject({ state: 'busy' })
  await first.close(async () => {})
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('abort during reservation reconciles a late commit before authority handoff', async () => {
  const host = workerHost(); global.SharedWorker = host.Worker
  const database = recordingDatabase(), controller = new AbortController(), opts = options(database)
  const first = (await createWebRecordingAuthority({ ...opts, signal: controller.signal }, 'id')).authority
  const input = declaration(first)
  database.pauseNext()
  const pending = first.declare(input)
  await database.requestsProcessed(); controller.abort()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  expect(first.current()).toBe(false)
  const closing = first.release(); database.complete(); await closing
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
  const next = (await createWebRecordingAuthority(opts, 'id')).authority
  expect(await next.declare(declaration(next))).toMatchObject({ ownerEpoch: '2' })
  await next.release()
})
