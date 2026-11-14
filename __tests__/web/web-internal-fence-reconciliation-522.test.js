const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
const { createWebRecordingPeerIdentityStore } = require('../../src/web/web-recording-peer-identity')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { webcrypto } = require('crypto')

function locks() {
  return { request(_name, _options, callback) { return Promise.resolve(callback({})) } }
}
const declaration = {
  id: 'internal-fence', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'generation',
  context: { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 1,
    members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: null, preference: null }] }
}
const result = { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] } }

function withholdCompletion(db, matches, limit = 1) {
  const original = db.transaction.bind(db)
  const held = []
  db.transaction = (...args) => {
    const tx = original(...args), objectStore = tx.objectStore.bind(tx)
    let selected = false
    tx.objectStore = name => {
      const store = objectStore(name)
      for (const method of ['get', 'put']) {
        const action = store[method]
        store[method] = value => { if (matches({ mode: args[1], name, method, value })) selected = true; return action(value) }
      }
      return store
    }
    let callback
    Object.defineProperty(tx, 'oncomplete', {
      set: fn => { callback = fn },
      get: () => () => { if (selected && held.length < limit) held.push(() => callback?.()); else callback?.() }
    })
    return tx
  }
  return held
}

async function setup() {
  const db = transactionalDatabase()
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: locks(), digest: async () => 'ef'.repeat(32) }, declaration, webcrypto, {
    store: createWebRecordingPeerIdentityStore(db),
    reference: { version: 1, scope: 'origin', backendId: 'web', opaqueId: 'peer' }
  })
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: false } })
  await bridge.retire('1')
  await bridge.seal()
  await bridge.observeTerminal({ result })
  return { db, bridge }
}

test.each(['terminal-read', 'fence-release'])('internal bridge bounds and reconciles a withheld %s completion', async stage => {
  const { db, bridge } = await setup()
  const held = withholdCompletion(db, x => stage === 'terminal-read'
    ? x.mode === 'readonly' && x.name === 'recordings' && x.method === 'get'
    : x.name === 'readerReceipts' && x.method === 'put' && x.value.state === 'released' && x.value.recordingId.startsWith(':ubm-owner-fence-v1:'))
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    let outcome = 'pending'
    bridge.releaseAuthority().then(() => { outcome = 'released' }, () => { outcome = 'unknown' })
    for (let i = 0; i < 30 && held.length === 0; i++) await jest.advanceTimersByTimeAsync(1000)
    expect(held).toHaveLength(1)
    await jest.advanceTimersByTimeAsync(6000)
    expect(outcome).toBe('unknown')
    const retry = bridge.releaseAuthority()
    for (let i = 0; i < 30; i++) await jest.advanceTimersByTimeAsync(1000)
    await expect(retry).resolves.toBeUndefined()
    expect(db.rows('readerReceipts').find(row => row.recordingId === ':ubm-owner-fence-v1:internal-fence').state).toBe('released')
  } finally { held.forEach(done => done()); jest.useRealTimers() }
})

test('internal bridge limits outstanding fence attempts and resumes from a late receipt', async () => {
  const { db, bridge } = await setup()
  const held = withholdCompletion(db, x => x.name === 'readerReceipts' && x.method === 'put' &&
    x.value.state === 'released' && x.value.recordingId.startsWith(':ubm-owner-fence-v1:'), 2)
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    for (let attempt = 0; attempt < 3; attempt++) {
      let outcome = 'pending'
      bridge.releaseAuthority().then(() => { outcome = 'released' }, () => { outcome = 'unknown' })
      await jest.advanceTimersByTimeAsync(6000)
      expect(outcome).toBe('unknown')
    }
    expect(held).toHaveLength(2)
    held[0]()
    await Promise.resolve()
    const retry = bridge.releaseAuthority()
    await jest.advanceTimersByTimeAsync(1000)
    await expect(retry).resolves.toBeUndefined()
    expect(db.rows('readerReceipts').find(row => row.recordingId === ':ubm-owner-fence-v1:internal-fence').state).toBe('released')
  } finally { held.forEach(done => done()); jest.useRealTimers() }
})

test('internal bridge leaves the fence held when the retained observation does not match', async () => {
  const { db, bridge } = await setup()
  db.mutate('recordings', row => { row.terminalObservation.operationId = 'f'.repeat(64) })
  await expect(bridge.releaseAuthority()).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
  expect(db.rows('readerReceipts').find(row => row.recordingId === ':ubm-owner-fence-v1:internal-fence').state).toBe('held')
})
