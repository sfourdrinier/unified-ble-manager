const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
const { createWebRecordingJournalBridge } = require('../../src/web/web-recording-journal-bridge')

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}

function fixture() {
  const pending = deferred()
  const entered = deferred()
  const events = []
  const committed = []
  const characteristic = {}
  const ingress = new WebNotificationIngress((_target, added) => events.push(added ? 'install' : 'remove'))
  const lease = { enable: jest.fn(async () => events.push('enable')), release: jest.fn(async () => {}) }
  let current = true
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'heart', characteristic, lease }], 4, 100, () => current)
  let declaration
  const bridge = createWebRecordingJournalBridge({}, { context: { version: 1 } }, async () => ({ current: () => true, release: async () => {} }), (_db, input) => {
    declaration = input
    return {
      declare: async () => { events.push('declare'); entered.resolve(); await pending.promise },
      open: async () => events.push('open'),
      commitWithDurability: async delivery => { committed.push(delivery); return { durability: 'default' } },
      retire: async () => {}, seal: async () => {}
    }
  })
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  return { pending, entered, events, committed, characteristic, ingress, lease, owner, bridge, supervisor,
    declaration: () => declaration, stale: () => { current = false } }
}

test('real supervisor and bridge declare reserved epochs before installing routes and record those same epochs', async () => {
  const f = fixture()
  const start = f.supervisor.start()
  await f.entered.promise
  expect(f.declaration().memberEpochs).toEqual([{ member: '0', epoch: '1' }])
  expect(f.events).toEqual(['declare'])
  f.ingress.admit(f.characteristic, new Uint8Array([9]))
  f.pending.resolve()
  expect((await start).state).toBe('active')
  expect(f.events).toEqual(['declare', 'open', 'install', 'enable'])
  f.ingress.admit(f.characteristic, new Uint8Array([2]))
  const sourceEpochs = f.owner.memberships()
  expect(sourceEpochs).toEqual(f.declaration().memberEpochs)
  f.supervisor.stop()
  await f.supervisor.retry()
  expect(f.committed.map(item => item.record.kind)).toEqual(['value', 'terminal'])
  expect(f.committed[0].record).toMatchObject({ member: '0', membership: { member: '0', epoch: '1' }, bytes: new Uint8Array([2]) })
})

test('cancellation while declaration is pending never opens, installs, or enables', async () => {
  const f = fixture()
  const abort = new AbortController()
  const start = f.supervisor.start(abort.signal)
  await f.entered.promise
  abort.abort()
  await expect(start).rejects.toMatchObject({ name: 'AbortError' })
  f.pending.resolve()
  for (let i = 0; i < 30; i++) await Promise.resolve()
  await f.supervisor.retry()
  expect(f.events).toEqual(['declare'])
  expect(f.supervisor.status()).toMatchObject({ state: 'finished', sourceStarted: false, terminalCommitted: false })
})

test('a stale graph after declaration is never installed or enabled', async () => {
  const f = fixture()
  const start = f.supervisor.start()
  await f.entered.promise
  f.stale()
  f.pending.resolve()
  expect((await start).state).toBe('failed')
  expect(f.events).toEqual(['declare', 'open'])
  expect(f.lease.enable).not.toHaveBeenCalled()
})

test('a second supervisor cannot release or install the first supervisor owner', async () => {
  const f = fixture()
  const first = f.supervisor.start()
  await f.entered.promise
  const secondJournal = { prepare: jest.fn(), releaseAuthority: jest.fn(), classifyFailure: () => 'failure' }
  const second = new WebOrderedRecordingSupervisor(f.owner, secondJournal)
  expect((await second.start()).state).toBe('failed')
  expect(secondJournal.prepare).not.toHaveBeenCalled()
  expect(secondJournal.releaseAuthority).not.toHaveBeenCalled()
  f.pending.resolve()
  expect((await first).state).toBe('active')
  expect(f.events).toEqual(['declare', 'open', 'install', 'enable'])
  f.supervisor.stop()
  await f.supervisor.retry()
})

test('declaration callback cannot change the membership passed to the journal', async () => {
  let declared
  const bridge = createWebRecordingJournalBridge({}, async memberships => {
    memberships[0].epoch = '999'
    return {}
  }, async () => ({ current: () => true, release: async () => {} }), (_db, input) => {
    declared = input.memberEpochs
    return { declare: async () => {}, open: async () => {} }
  })
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '7' }] })
  expect(declared).toEqual([{ member: '0', epoch: '7' }])
})

test('a rejected declaration never installs routes or dispatches an enable', async () => {
  const f = fixture()
  const start = f.supervisor.start()
  await f.entered.promise
  f.pending.reject(new Error('declaration failed'))
  expect((await start).state).toBe('failed')
  await f.supervisor.retry()
  expect(f.events).toEqual(['declare'])
  expect(f.lease.enable).not.toHaveBeenCalled()
  expect(f.owner.isInstalled()).toBe(false)
})
