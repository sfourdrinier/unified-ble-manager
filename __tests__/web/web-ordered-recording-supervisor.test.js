const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}

function fixture() {
  const events = []
  const ingress = new WebNotificationIngress()
  const a = {}, b = {}
  let wake, source
  const journal = {
    prepare: jest.fn(async () => { events.push('prepare') }),
    open: jest.fn(async () => { events.push('open') }),
    commit: jest.fn(async delivery => { events.push(`commit:${delivery.id}:${delivery.record.kind}`) }),
    retire: jest.fn(async id => { events.push(`retire:${id}`) }),
    seal: jest.fn(async () => { events.push('seal') }),
    releaseAuthority: jest.fn(async () => { events.push('authority') }),
    classifyFailure: () => 'failure',
    isDefinitiveFailure: () => true
  }
  const leases = ['a', 'b'].map(id => ({
    id,
    enable: jest.fn(async () => { events.push(`enable:${id}`) }),
    release: jest.fn(async () => { events.push(`release:${id}`) })
  }))
  const owner = {
    reserve: jest.fn(() => ({ memberships: () => [{ member: 'a', epoch: '1' }, { member: 'b', epoch: '2' }] })),
    install: jest.fn(callback => {
      events.push('install')
      wake = callback
      source = ingress.registerRetained(new Map([[a, 'a'], [b, 'b']]), 4, 20)
      return { source, leases }
    }),
    release: jest.fn(async () => { events.push('source-release') }),
    isCurrent: jest.fn(() => true)
  }
  const supervisor = new WebOrderedRecordingSupervisor(owner, journal)
  return { supervisor, journal, owner, events, leases, get source() { return source },
    admit(characteristic, bytes) { ingress.admit(characteristic, new Uint8Array(bytes)); wake() }, a, b }
}

test('storage readiness recovery survives a later member cleanup observation', async () => {
  jest.useFakeTimers()
  try {
    const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
    const f = fixture()
    const cleanup = deferred(), commit = deferred()
    const failure = new Error('member cleanup failed')
    f.leases[0].release.mockImplementationOnce(() => cleanup.promise)
    f.journal.commit.mockImplementationOnce(() => commit.promise)
    f.journal.classifyFailure = () => { cleanup.reject(failure); return 'failure' }
    await f.supervisor.start()
    f.admit(f.a, [7])
    f.supervisor.stop()
    for (let i = 0; i < 100 && !f.journal.commit.mock.calls.length; i++) await Promise.resolve()
    for (let i = 0; i < 100 && !f.leases[0].release.mock.calls.length; i++) await Promise.resolve()
    expect(f.leases[0].release).toHaveBeenCalledTimes(1)
    expect(jest.getTimerCount()).toBe(1) // only the in-flight storage timeout
    commit.reject(new WebRecordingReadinessError(new Error('storage unavailable')))
    for (let i = 0; i < 200; i++) await Promise.resolve()
    expect(f.supervisor.status().runtimeFailure.error).toBe(failure)
    await jest.advanceTimersByTimeAsync(6000)
    for (let i = 0; i < 200; i++) await Promise.resolve()
    expect(f.supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true, authorityReleased: true })
    expect(f.journal.commit.mock.calls.map(([delivery]) => delivery.record.kind)).toEqual(['value', 'value', 'terminal'])
    expect(f.leases[0].release).toHaveBeenCalledTimes(2)
    expect(f.journal.retire).toHaveBeenCalledTimes(2)
  } finally { jest.useRealTimers() }
})

test.each(['reservation', 'retained', 'authority'])('records %s cleanup failure and retains it after retry', async phase => {
  const f = fixture()
  const failure = new Error(`${phase} cleanup failed`)
  if (phase === 'reservation') {
    f.journal.open.mockRejectedValueOnce(new Error('open aborted'))
    f.owner.release.mockRejectedValueOnce(failure)
  } else if (phase === 'retained') {
    f.owner.releaseRetainingDeliveries = jest.fn().mockRejectedValueOnce(failure).mockResolvedValue()
    f.journal.commit.mockRejectedValueOnce(new Error('storage failed'))
  } else {
    f.journal.open.mockRejectedValueOnce(new Error('open aborted'))
    f.owner.release.mockImplementationOnce(async () => { f.supervisor.loseAuthority() })
    f.journal.releaseAuthority.mockRejectedValueOnce(failure)
  }
  await f.supervisor.start()
  if (phase === 'retained') { f.admit(f.a, [1]) }
  for (let i = 0; i < 200 && f.supervisor.status().cleanupFailure !== failure; i++) await Promise.resolve()
  expect(f.supervisor.status().runtimeFailure).toEqual({ error: failure, persisted: false })
  expect(f.supervisor.status().runtimeFailurePhase).toBe('cleanup')
  await f.supervisor.retry()
  expect(f.supervisor.status().runtimeFailure).toEqual({ error: failure, persisted: false })
  expect(f.supervisor.status().runtimeFailurePhase).toBe('cleanup')
})

test.each([
  ['reserve', 'setup'],
  ['prepare', 'other'],
  ['open', 'other'],
  ['install', 'setup'],
  ['enable', 'setup']
])('classifies %s failure in its actual phase', async (point, phase) => {
  const f = fixture()
  const failure = new Error(`${point} failed`)
  if (point === 'reserve') f.owner.reserve.mockImplementationOnce(() => { throw failure })
  if (point === 'prepare') f.journal.prepare.mockRejectedValueOnce(failure)
  if (point === 'open') f.journal.open.mockRejectedValueOnce(failure)
  if (point === 'install') f.owner.install.mockImplementationOnce(() => { throw failure })
  if (point === 'enable') f.leases[0].enable.mockRejectedValueOnce(failure)
  const status = await f.supervisor.start()
  expect(status.setupFailure).toBe(failure)
  expect(status.setupFailurePhase).toBe(phase)
  expect(status.runtimeFailure).toEqual({ error: failure, persisted: false })
  expect(status.runtimeFailurePhase).toBe(phase)
  await f.supervisor.retry()
})

test('opens and installs before enabling and commits setup callbacks in ingress order', async () => {
  const f = fixture()
  f.leases[0].enable.mockImplementation(async () => { f.events.push('enable:a'); f.admit(f.b, [2]); f.admit(f.a, [1]) })
  expect((await f.supervisor.start()).state).toBe('active')
  await f.supervisor.retry()
  expect(f.events.slice(0, 4)).toEqual(['prepare', 'open', 'install', 'enable:a'])
  expect(f.journal.commit.mock.calls.map(([d]) => d.record.memberId)).toEqual(['b', 'a'])
  expect(f.source.peek()).toBeNull()
})

test('supervisor waits for exact asynchronous acknowledgement evidence before retiring the journal identity', async () => {
  const f = fixture()
  const evidence = deferred()
  f.journal.commit.mockResolvedValue(42)
  f.owner.acknowledged = jest.fn(() => evidence.promise)
  await f.supervisor.start()
  f.admit(f.a, [7])
  while (f.owner.acknowledged.mock.calls.length === 0) await Promise.resolve()
  expect(f.owner.acknowledged).toHaveBeenCalledWith(expect.objectContaining({ id: '1', journalOrdinal: 42 }))
  expect(f.journal.retire).not.toHaveBeenCalled()
  evidence.resolve()
  await f.supervisor.retry()
  expect(f.journal.retire).toHaveBeenCalledWith('1')
})

test('fences immediately and waits for exact commit before source acknowledgement and cleanup', async () => {
  const f = fixture()
  await f.supervisor.start()
  const pending = deferred()
  f.journal.commit.mockImplementationOnce(() => pending.promise)
  f.admit(f.a, [7])
  await Promise.resolve()
  f.supervisor.stop()
  expect(f.source.terminal).toBe(true)
  expect(f.source.peek().id).toBe('1')
  expect(f.owner.release).not.toHaveBeenCalled()
  pending.resolve()
  await f.supervisor.retry()
  expect(f.journal.commit.mock.calls.map(([d]) => d.record.kind)).toEqual(['value', 'terminal'])
  expect(f.events.indexOf('release:a')).toBeLessThan(f.events.indexOf('commit:2:terminal'))
  expect(f.events.indexOf('release:b')).toBeLessThan(f.events.indexOf('commit:2:terminal'))
  expect(f.events.indexOf('commit:2:terminal')).toBeLessThan(f.events.indexOf('seal'))
  expect(f.events.indexOf('release:b')).toBeLessThan(f.events.indexOf('source-release'))
  expect(f.events.indexOf('source-release')).toBeLessThan(f.events.indexOf('authority'))
  expect(f.supervisor.status().state).toBe('finished')
})

test('retains the explicitly observed final result after source cleanup and before authority release', async () => {
  const f = fixture()
  const result = { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] } }
  const observe = jest.fn(async () => { f.events.push('observe'); return { result } })
  f.journal.observeTerminal = jest.fn(async value => { f.events.push('persist'); expect(value.result).toEqual(result) })
  const supervisor = new WebOrderedRecordingSupervisor(f.owner, f.journal, observe)
  await supervisor.start()
  supervisor.stop()
  await supervisor.retry()
  expect(f.events.indexOf('source-release')).toBeLessThan(f.events.indexOf('observe'))
  expect(f.events.indexOf('observe')).toBeLessThan(f.events.indexOf('persist'))
  expect(f.events.indexOf('persist')).toBeLessThan(f.events.indexOf('authority'))
  expect(supervisor.status().state).toBe('finished')
})

test('retries the same observation after storage failure and keeps authority until it succeeds', async () => {
  const f = fixture()
  const observation = { result: { recording: { version: 1, state: 'interrupted', error: null }, hostCleanup: { state: 'uncertain' } } }
  const observe = jest.fn(async () => observation)
  f.journal.observeTerminal = jest.fn().mockRejectedValueOnce(new Error('storage unavailable')).mockResolvedValueOnce()
  const supervisor = new WebOrderedRecordingSupervisor(f.owner, f.journal, observe)
  await supervisor.start()
  supervisor.stop()
  await supervisor.retry()
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
  expect(supervisor.status().cleanupFailure).not.toBeNull()
  expect(supervisor.status().runtimeFailurePhase).toBe('other')
  await supervisor.retry()
  expect(observe).toHaveBeenCalledTimes(1)
  expect(f.journal.observeTerminal.mock.calls).toEqual([[observation], [observation]])
  expect(f.journal.releaseAuthority).toHaveBeenCalledTimes(1)
})

test('authority loss during observation prevents authority release after a late receipt', async () => {
  const f = fixture()
  const pending = deferred()
  f.journal.observeTerminal = jest.fn(() => pending.promise)
  const supervisor = new WebOrderedRecordingSupervisor(f.owner, f.journal, async () => ({ result: {
    recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'uncertain' }
  } }))
  await supervisor.start()
  supervisor.stop()
  while (!f.journal.observeTerminal.mock.calls.length) await Promise.resolve()
  supervisor.loseAuthority()
  pending.resolve()
  await supervisor.retry()
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
  expect(supervisor.status().state).toBe('failed')
})

test('capacity fences admission and retries the identical head before the retained tail', async () => {
  const f = fixture()
  f.journal.classifyFailure = () => 'capacity'
  await f.supervisor.start()
  f.journal.commit.mockRejectedValueOnce(new Error('full'))
  f.admit(f.a, [5])
  await f.supervisor.retry()
  expect(f.source.terminal).toBe(true)
  expect(f.source.peek().id).toBe('1')
  expect(f.supervisor.status().state).toBe('waiting-capacity')
  expect(f.supervisor.status().terminalCommitted).toBe(false)
  expect(f.owner.release).not.toHaveBeenCalled()
  await f.supervisor.retry()
  expect(f.journal.commit.mock.calls[1][0]).toEqual(f.journal.commit.mock.calls[0][0])
  expect(f.journal.commit.mock.calls.map(([d]) => d.record.kind)).toEqual(['value', 'value', 'terminal'])
  expect(f.supervisor.status().state).toBe('finished')
})

test('setup failure retains its cause and releases only dispatched members after draining terminal', async () => {
  const f = fixture()
  const error = new Error('enable failed')
  f.leases[0].enable.mockImplementation(async () => { f.admit(f.a, [3]); throw error })
  const result = await f.supervisor.start()
  await f.supervisor.retry()
  expect(result.failure).toBe(error)
  expect(f.supervisor.status().failure).toBe(error)
  expect(f.journal.commit.mock.calls.map(([d]) => d.record.kind)).toEqual(['value', 'terminal'])
  expect(f.leases[0].release).toHaveBeenCalledTimes(1)
  expect(f.leases[1].enable).not.toHaveBeenCalled()
  expect(f.leases[1].release).not.toHaveBeenCalled()
  expect(f.supervisor.status().members[0]).toMatchObject({ acquisition: 'failed', acquisitionFailure: error, cleanup: 'released' })
  expect(f.supervisor.status().members[1]).toMatchObject({ acquisition: 'pending', cleanup: 'not-needed' })
})

test('setup and storage failures remain separately visible with truthful terminal evidence', async () => {
  const f = fixture()
  const setupError = new Error('enable failed')
  const storageError = new Error('storage failed')
  f.journal.classifyFailure = () => 'failure'
  f.journal.commit.mockRejectedValueOnce(storageError)
  f.leases[0].enable.mockRejectedValueOnce(setupError)
  await f.supervisor.start()
  for (let i = 0; i < 100 && f.supervisor.status().runtimeFailure?.error !== storageError; i++) await Promise.resolve()
  expect(f.supervisor.status()).toMatchObject({
    setupFailure: setupError,
    runtimeFailure: { error: storageError, persisted: false }
  })
  await f.supervisor.retry()
  expect(f.supervisor.status().setupFailure).toBe(setupError)
})

test('stop during pending enable retains ownership and compensates the late success', async () => {
  const f = fixture()
  const enable = deferred()
  f.leases[0].enable.mockReturnValue(enable.promise)
  const start = f.supervisor.start()
  while (!f.leases[0].enable.mock.calls.length) await Promise.resolve()
  expect(f.supervisor.stop().state).toBe('waiting-source')
  await f.supervisor.retry()
  expect(f.owner.release).not.toHaveBeenCalled()
  enable.resolve()
  await start
  await f.supervisor.retry()
  expect(f.leases[0].release).toHaveBeenCalledTimes(1)
  expect(f.leases[1].enable).not.toHaveBeenCalled()
  expect(f.supervisor.status().state).toBe('finished')
})

test('pre-abort does nothing while an aborted waiter leaves the owner running', async () => {
  const f = fixture()
  const before = new AbortController()
  before.abort()
  await expect(f.supervisor.start(before.signal)).rejects.toMatchObject({ name: 'AbortError' })
  expect(f.journal.prepare).not.toHaveBeenCalled()
  const enable = deferred()
  f.leases[0].enable.mockReturnValue(enable.promise)
  const controller = new AbortController()
  const waiter = f.supervisor.start(controller.signal)
  while (!f.leases[0].enable.mock.calls.length) await Promise.resolve()
  controller.abort()
  await expect(waiter).rejects.toMatchObject({ name: 'AbortError' })
  enable.resolve()
  expect((await f.supervisor.start()).state).toBe('active')
  f.supervisor.stop()
  await f.supervisor.retry()
})

test('the last waiter aborting before source admission prevents enable', async () => {
  const f = fixture()
  const prepare = deferred()
  f.journal.prepare.mockReturnValue(prepare.promise)
  const controller = new AbortController()
  const waiter = f.supervisor.start(controller.signal)
  controller.abort()
  await expect(waiter).rejects.toMatchObject({ name: 'AbortError' })
  prepare.resolve()
  await f.supervisor.retry()
  await f.supervisor.retry()
  expect(f.owner.install).not.toHaveBeenCalled()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
})

test('another waiter keeps a pre-admission start alive when one waiter aborts', async () => {
  const f = fixture()
  const prepare = deferred()
  f.journal.prepare.mockReturnValue(prepare.promise)
  const controller = new AbortController()
  const first = f.supervisor.start(controller.signal)
  const second = f.supervisor.start()
  controller.abort()
  await expect(first).rejects.toMatchObject({ name: 'AbortError' })
  prepare.resolve()
  expect((await second).state).toBe('active')
  f.supervisor.stop()
  await f.supervisor.retry()
})

test('terminal commit remains truthful when subsequent identity retirement fails', async () => {
  const f = fixture()
  f.journal.classifyFailure = () => 'failure'
  await f.supervisor.start()
  f.journal.retire.mockRejectedValueOnce(new Error('retirement failed'))
  f.supervisor.stop()
  for (let i = 0; i < 100 && f.supervisor.status().state !== 'failed'; i++) await Promise.resolve()
  expect(f.supervisor.status().terminalCommitted).toBe(true)
  expect(f.supervisor.status().state).toBe('failed')
  expect(f.owner.release).not.toHaveBeenCalled()
  await f.supervisor.retry()
  expect(f.supervisor.status().state).toBe('finished')
  expect(f.journal.commit).toHaveBeenCalledTimes(1)
})

test('an ambiguous source-open outcome is not reissued or treated as physical start', async () => {
  const f = fixture()
  f.journal.isDefinitiveFailure = () => false
  const error = new Error('open outcome unknown')
  f.journal.open.mockRejectedValueOnce(error)
  expect((await f.supervisor.start()).sourceStarted).toBe(false)
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  await f.supervisor.retry()
  await f.supervisor.retry()
  expect(f.journal.open).toHaveBeenCalledTimes(1)
  expect(f.journal.commit).not.toHaveBeenCalled()
  expect(f.owner.release).toHaveBeenCalledTimes(1)
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
  expect(f.owner.install).not.toHaveBeenCalled()
  expect(f.supervisor.status().failure).toBe(error)
})

test('authority loss fences immediately and prevents additional owner commits', async () => {
  const f = fixture()
  await f.supervisor.start()
  f.supervisor.loseAuthority()
  f.admit(f.a, [9])
  await f.supervisor.retry()
  expect(f.source.terminal).toBe(true)
  expect(f.journal.commit).not.toHaveBeenCalled()
  expect(f.leases[0].release).toHaveBeenCalled()
  expect(f.owner.release).not.toHaveBeenCalled()
  expect(f.supervisor.status().state).toBe('failed')
})

test('restoration fences when connection or graph revalidation fails', async () => {
  const f = fixture()
  await f.supervisor.start()
  f.owner.isCurrent.mockReturnValue(false)
  expect(f.supervisor.restore()).toBe(false)
  expect(f.source.terminal).toBe(true)
  await f.supervisor.retry()
  expect(f.journal.commit.mock.calls[0][0].record.reason).toBe('connection-lost')
})

test('refuses duplicate member identities before any enable and drains the fenced source', async () => {
  const f = fixture()
  f.leases[1].id = f.leases[0].id
  const start = await f.supervisor.start()
  expect(start.failure).not.toBeNull()
  expect(start.sourceStarted).toBe(false)
  await f.supervisor.retry()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  expect(f.leases[1].enable).not.toHaveBeenCalled()
})

test('source start reports dispatch and its result independently of durable open', async () => {
  const f = fixture()
  const enable = deferred()
  f.leases[0].enable.mockReturnValue(enable.promise)
  const start = f.supervisor.start()
  while (!f.leases[0].enable.mock.calls.length) await Promise.resolve()
  expect(f.supervisor.status().sourceStarted).toBe('unknown')
  enable.resolve()
  expect((await start).sourceStarted).toBe(true)
  f.supervisor.stop()
  await f.supervisor.retry()
})

test('a wake between the final pass and work settlement schedules another pass', async () => {
  const f = fixture()
  const gate = deferred()
  let passes = 0
  f.supervisor.advance = jest.fn(() => {
    passes += 1
    return passes === 1 ? gate.promise : Promise.resolve()
  })
  f.supervisor.wake()
  await Promise.resolve()
  gate.resolve()
  queueMicrotask(() => f.supervisor.wake())
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(passes).toBe(2)
})

test('an unresolved commit fences and cleans physical leases without retrying or acknowledging its head', async () => {
  jest.useFakeTimers()
  try {
    const f = fixture()
    await f.supervisor.start()
    const pending = deferred()
    f.journal.commit.mockImplementationOnce(() => pending.promise)
    f.admit(f.a, [7, 8])
    while (!f.journal.commit.mock.calls.length) await Promise.resolve()
    jest.advanceTimersByTime(30000)
    for (let i = 0; i < 100 && !f.leases[1].release.mock.calls.length; i++) await Promise.resolve()
    expect(f.source.terminal).toBe(true)
    expect(f.source.peek()).toMatchObject({ id: '1', record: { value: new Uint8Array([7, 8]) } })
    expect(f.leases[0].release).toHaveBeenCalledTimes(1)
    expect(f.leases[1].release).toHaveBeenCalledTimes(1)
    expect(f.owner.release).not.toHaveBeenCalled()
    expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
    expect(f.supervisor.status().failure.message).toMatch(/unknown/i)
    await f.supervisor.retry()
    expect(f.journal.commit).toHaveBeenCalledTimes(1)
    pending.resolve()
    for (let i = 0; i < 100 && f.supervisor.status().state !== 'finished'; i++) await Promise.resolve()
    expect(f.journal.commit.mock.calls.map(([delivery]) => delivery.record.kind)).toEqual(['value', 'terminal'])
    expect(f.supervisor.status().state).toBe('finished')
  } finally {
    jest.useRealTimers()
  }
})

test('a lost seal completion cannot keep physical leases enabled', async () => {
  jest.useFakeTimers()
  try {
    const f = fixture()
    await f.supervisor.start()
    const pending = deferred()
    f.journal.seal.mockReturnValueOnce(pending.promise)
    f.supervisor.stop()
    while (!f.journal.seal.mock.calls.length) await Promise.resolve()
    jest.advanceTimersByTime(30000)
    for (let i = 0; i < 100 && !f.leases[1].release.mock.calls.length; i++) await Promise.resolve()
    expect(f.leases[0].release).toHaveBeenCalledTimes(1)
    expect(f.leases[1].release).toHaveBeenCalledTimes(1)
    expect(f.owner.release).not.toHaveBeenCalled()
    await f.supervisor.retry()
    expect(f.journal.seal).toHaveBeenCalledTimes(1)
    pending.resolve()
    for (let i = 0; i < 100 && f.supervisor.status().state !== 'finished'; i++) await Promise.resolve()
    expect(f.supervisor.status().state).toBe('finished')
  } finally {
    jest.useRealTimers()
  }
})

test('a late rejected receipt remains unknown and visible without a second commit', async () => {
  jest.useFakeTimers()
  try {
    const f = fixture()
    f.journal.isDefinitiveFailure = () => false
    await f.supervisor.start()
    const pending = deferred()
    const lateError = new Error('late transport error')
    f.journal.commit.mockReturnValueOnce(pending.promise)
    f.admit(f.a, [9])
    while (!f.journal.commit.mock.calls.length) await Promise.resolve()
    jest.advanceTimersByTime(30000)
    for (let i = 0; i < 100 && !f.leases[1].release.mock.calls.length; i++) await Promise.resolve()
    pending.reject(lateError)
    for (let i = 0; i < 20; i++) await Promise.resolve()
    expect(f.supervisor.status().runtimeFailure.error.errors).toContain(lateError)
    await f.supervisor.retry()
    expect(f.journal.commit).toHaveBeenCalledTimes(1)
    expect(f.source.peek().id).toBe('1')
    expect(f.owner.release).not.toHaveBeenCalled()
  } finally {
    jest.useRealTimers()
  }
})

test('an ambiguous rejected commit is retained without reissuing the write', async () => {
  const f = fixture()
  f.journal.isDefinitiveFailure = () => false
  await f.supervisor.start()
  const error = new Error('commit response lost')
  f.journal.commit.mockRejectedValueOnce(error)
  f.admit(f.a, [6])
  for (let i = 0; i < 100 && !f.leases[1].release.mock.calls.length; i++) await Promise.resolve()
  await f.supervisor.retry()
  expect(f.journal.commit).toHaveBeenCalledTimes(1)
  expect(f.source.peek().id).toBe('1')
  expect(f.owner.release).not.toHaveBeenCalled()
  expect(f.supervisor.status().failure.errors).toContain(error)
})

test('authority loss during a commit prevents a late success from acknowledging its head', async () => {
  const f = fixture()
  await f.supervisor.start()
  const pending = deferred()
  f.journal.commit.mockReturnValueOnce(pending.promise)
  f.admit(f.a, [4])
  while (!f.journal.commit.mock.calls.length) await Promise.resolve()
  f.supervisor.loseAuthority()
  pending.resolve()
  await f.supervisor.retry()
  expect(f.source.peek().id).toBe('1')
  expect(f.journal.retire).not.toHaveBeenCalled()
  expect(f.owner.release).not.toHaveBeenCalled()
})

test('a source already terminal when installation returns never enables a member', async () => {
  const f = fixture()
  const install = f.owner.install.getMockImplementation()
  f.owner.install.mockImplementation(wake => {
    const installed = install(wake)
    installed.source.retire('connection-lost')
    return installed
  })
  await f.supervisor.start()
  await f.supervisor.retry()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  expect(f.journal.commit.mock.calls.map(([d]) => d.record.kind)).toEqual(['terminal'])
})

test('a source that omits its terminal cannot be sealed or released', async () => {
  const f = fixture()
  const install = f.owner.install.getMockImplementation()
  f.owner.install.mockImplementation(wake => {
    const installed = install(wake)
    return { ...installed, source: { ...installed.source, peek: () => null } }
  })
  await f.supervisor.start()
  f.supervisor.stop()
  await f.supervisor.retry()
  expect(f.supervisor.status().terminalCommitted).toBe(false)
  expect(f.journal.seal).not.toHaveBeenCalled()
  expect(f.owner.release).not.toHaveBeenCalled()
  expect(f.supervisor.status().state).toBe('failed')
})

test('failed restoration checks fence admission even when revalidation throws', async () => {
  const f = fixture()
  await f.supervisor.start()
  f.owner.isCurrent.mockImplementation(() => { throw new Error('graph unavailable') })
  expect(f.supervisor.restore()).toBe(false)
  expect(f.source.terminal).toBe(true)
  await f.supervisor.retry()
})

test('overflow persists exact known loss and terminal; hidden page alone does not stop', async () => {
  const f = fixture()
  await f.supervisor.start()
  f.supervisor.pagehide(false)
  for (let i = 0; i < 5; i++) f.admit(f.a, [i, i])
  await f.supervisor.retry()
  expect(f.journal.commit.mock.calls.map(([d]) => d.record)).toContainEqual({ kind: 'loss', lostValues: 1, lostBytes: 2 })
  expect(f.journal.commit.mock.calls.map(([d]) => d.record)).toContainEqual({ kind: 'terminal', reason: 'overflow', uncertainTail: true })
  expect(f.supervisor.status().state).toBe('finished')
})

test('failed member cleanup is retained and retried before releasing source authority', async () => {
  const f = fixture()
  await f.supervisor.start()
  f.leases[0].release.mockRejectedValueOnce(new Error('stop failed'))
  f.supervisor.stop()
  await f.supervisor.retry()
  expect(f.supervisor.status().state).toBe('failed')
  expect(f.journal.commit.mock.calls.map(([delivery]) => delivery.record.kind)).not.toContain('terminal')
  expect(f.owner.release).not.toHaveBeenCalled()
  await f.supervisor.retry()
  expect(f.leases[0].release).toHaveBeenCalledTimes(2)
  expect(f.leases[1].release).toHaveBeenCalledTimes(1)
  expect(f.supervisor.status().state).toBe('finished')
})

test('a proven aborted open cleans quiescent setup and releases authority without enabling', async () => {
  const f = fixture()
  const failure = new Error('open aborted')
  f.journal.open.mockRejectedValueOnce(failure)
  await f.supervisor.start()
  await f.supervisor.retry()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  expect(f.owner.release).toHaveBeenCalledTimes(1)
  expect(f.journal.releaseAuthority).toHaveBeenCalledTimes(1)
  expect(f.supervisor.status().setupFailure).toBe(failure)
})

test('authority loss detaches a fenced source while preserving its unacknowledged FIFO', async () => {
  const f = fixture()
  f.owner.releaseRetainingDeliveries = jest.fn(async () => {})
  await f.supervisor.start()
  f.supervisor.loseAuthority()
  await f.supervisor.retry()
  expect(f.owner.releaseRetainingDeliveries).toHaveBeenCalledTimes(1)
  expect(f.source.peek()).toMatchObject({ id: '1', record: { kind: 'terminal' } })
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
})

test('lost open completion fences on timeout, retains evidence, and accepts a late completion without enabling', async () => {
  jest.useFakeTimers()
  try {
    const f = fixture()
    const pending = deferred()
    f.owner.releaseRetainingDeliveries = jest.fn(async () => {})
    f.journal.open.mockReturnValueOnce(pending.promise)
    const start = f.supervisor.start()
    while (!f.journal.open.mock.calls.length) await Promise.resolve()
    jest.advanceTimersByTime(30000)
    await start
    await f.supervisor.retry()
    expect(f.leases[0].enable).not.toHaveBeenCalled()
    expect(f.journal.open).toHaveBeenCalledTimes(1)
    expect(f.owner.install).not.toHaveBeenCalled()
    expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
    pending.resolve()
    for (let i = 0; i < 30; i++) await Promise.resolve()
    expect(f.journal.commit).not.toHaveBeenCalled()
    expect(f.supervisor.status().state).toBe('failed')
  } finally { jest.useRealTimers() }
})

test('retries an exact open reconciliation without issuing a second open', async () => {
  const f = fixture()
  f.journal.isDefinitiveFailure = () => false
  f.journal.open.mockRejectedValueOnce(new Error('reply lost'))
  f.journal.reconcileOpen = jest.fn().mockResolvedValueOnce('unknown').mockResolvedValueOnce('committed')
  await f.supervisor.start()
  await f.supervisor.retry()
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
  expect(f.journal.commit).not.toHaveBeenCalled()
  await f.supervisor.retry()
  expect(f.journal.open).toHaveBeenCalledTimes(1)
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  expect(f.journal.commit).not.toHaveBeenCalled()
  expect(f.supervisor.status().state).toBe('failed')
})

test('authority loss during open leaves ingress uninstalled', async () => {
  const f = fixture()
  const pending = deferred()
  f.journal.open.mockReturnValueOnce(pending.promise)
  f.owner.releaseRetainingDeliveries = jest.fn(async () => {})
  const start = f.supervisor.start()
  while (!f.journal.open.mock.calls.length) await Promise.resolve()
  f.supervisor.loseAuthority()
  for (let i = 0; i < 30 && !f.owner.releaseRetainingDeliveries.mock.calls.length; i++) await Promise.resolve()
  expect(f.owner.install).not.toHaveBeenCalled()
  expect(f.owner.release).toHaveBeenCalledTimes(1)
  pending.resolve()
  await start
  await f.supervisor.retry()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
})

test('stop while open is pending leaves ingress uninstalled and retains unresolved open', async () => {
  const f = fixture()
  const pending = deferred()
  f.journal.open.mockReturnValueOnce(pending.promise)
  f.owner.releaseRetainingDeliveries = jest.fn(async () => {})
  const start = f.supervisor.start()
  while (!f.journal.open.mock.calls.length) await Promise.resolve()
  f.supervisor.stop()
  for (let i = 0; i < 30 && !f.owner.releaseRetainingDeliveries.mock.calls.length; i++) await Promise.resolve()
  expect(f.owner.install).not.toHaveBeenCalled()
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
  pending.resolve()
  await start
  await f.supervisor.retry()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  expect(f.journal.commit).not.toHaveBeenCalled()
  expect(f.supervisor.status().state).toBe('failed')
})

test('a blocked open reconciliation is bounded and cannot create repeated barrier tasks', async () => {
  jest.useFakeTimers()
  try {
    const f = fixture()
    f.journal.isDefinitiveFailure = () => false
    f.journal.open.mockRejectedValueOnce(new Error('reply lost'))
    const barrier = deferred()
    f.journal.reconcileOpen = jest.fn(() => barrier.promise)
    await f.supervisor.start()
    const retry = f.supervisor.retry()
    while (!f.journal.reconcileOpen.mock.calls.length) await Promise.resolve()
    jest.advanceTimersByTime(30000)
    await retry
    expect(f.journal.open).toHaveBeenCalledTimes(1)
    expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
    barrier.resolve('committed')
    await f.supervisor.retry()
    expect(f.journal.reconcileOpen).toHaveBeenCalledTimes(1)
    expect(f.supervisor.status().state).toBe('failed')
  } finally { jest.useRealTimers() }
})

test('an aborted open can finish cleanup after an earlier stop', async () => {
  const f = fixture()
  const pending = deferred()
  f.journal.open.mockReturnValueOnce(pending.promise)
  f.owner.releaseRetainingDeliveries = jest.fn(async () => {})
  const start = f.supervisor.start()
  while (!f.journal.open.mock.calls.length) await Promise.resolve()
  f.supervisor.stop()
  for (let i = 0; i < 30 && !f.owner.releaseRetainingDeliveries.mock.calls.length; i++) await Promise.resolve()
  pending.reject(new Error('aborted'))
  await start
  await f.supervisor.retry()
  expect(f.owner.install).not.toHaveBeenCalled()
  expect(f.journal.releaseAuthority).toHaveBeenCalledTimes(1)
  expect(f.supervisor.status().state).toBe('finished')
})

test('authority loss cleans already acquired members while a later enable remains pending', async () => {
  const f = fixture()
  const pending = deferred()
  f.leases[1].enable.mockReturnValueOnce(pending.promise)
  f.owner.releaseRetainingDeliveries = jest.fn(async () => {})
  const start = f.supervisor.start()
  while (!f.leases[1].enable.mock.calls.length) await Promise.resolve()
  f.supervisor.loseAuthority()
  for (let i = 0; i < 30 && !f.leases[0].release.mock.calls.length; i++) await Promise.resolve()
  expect(f.leases[0].release).toHaveBeenCalledTimes(1)
  expect(f.leases[1].release).not.toHaveBeenCalled()
  expect(f.owner.releaseRetainingDeliveries).not.toHaveBeenCalled()
  pending.resolve()
  await start
  await f.supervisor.retry()
  expect(f.leases[1].release).toHaveBeenCalledTimes(1)
  expect(f.owner.releaseRetainingDeliveries).toHaveBeenCalledTimes(1)
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
})

test('malformed open reconciliation cannot authorize release or a second open', async () => {
  const f = fixture()
  f.journal.isDefinitiveFailure = () => false
  f.journal.open.mockRejectedValueOnce(new Error('lost'))
  f.journal.reconcileOpen = jest.fn(async () => 'unexpected')
  await f.supervisor.start()
  await f.supervisor.retry()
  expect(f.journal.open).toHaveBeenCalledTimes(1)
  expect(f.journal.releaseAuthority).not.toHaveBeenCalled()
  expect(f.owner.install).not.toHaveBeenCalled()
})

test('does not install source while the journal declaration and open are pending', async () => {
  const f = fixture()
  const pending = deferred()
  f.journal.open.mockReturnValueOnce(pending.promise)
  const start = f.supervisor.start()
  while (!f.journal.open.mock.calls.length) await Promise.resolve()
  expect(f.owner.install).not.toHaveBeenCalled()
  expect(f.leases[0].enable).not.toHaveBeenCalled()
  pending.resolve()
  await start
  expect(f.owner.install).toHaveBeenCalledTimes(1)
})
