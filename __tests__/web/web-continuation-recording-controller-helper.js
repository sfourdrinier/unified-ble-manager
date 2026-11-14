// Deterministic transaction scheduler: overlapping transactions serialize and aborts roll back.
function recordingDatabase() {
  const names = [
    'recordings',
    'records',
    'pendingIdentities',
    'readerReceipts',
    'authorityReservations',
    'abandonments'
  ]
  const stores = new Map(names.map(name => [name, new Map()]))
  stores.get('readerReceipts').set(JSON.stringify(':ubm-authority-policy-v1'), {
    recordingId: ':ubm-authority-policy-v1',
    version: 2,
    protocol: 'ubm-authority-reservations/1'
  })
  const clone = value => (value === undefined ? undefined : structuredClone(value))
  const keyFor = (name, row) =>
    JSON.stringify(
      name === 'recordings' || name === 'authorityReservations'
        ? row.id
        : name === 'records'
          ? [row.recordingId, row.ordinal]
          : name === 'readerReceipts'
            ? row.recordingId
            : [row.recordingId, row.producerIncarnation, row.collectionId, row.sourceGeneration, row.deliveryId]
    )
  let tail = Promise.resolve(),
    failure,
    hold,
    processed
  return {
    rows: name => [...stores.get(name).values()].map(clone),
    mutate: (name, fn) => {
      for (const row of stores.get(name).values()) fn(row)
    },
    remove: name => stores.get(name).clear(),
    abortNext: error => {
      failure = error
    },
    pauseNext: () => {
      processed = new Promise(resolve => {
        hold = { processed: resolve }
      })
    },
    requestsProcessed: () => processed,
    complete: () => hold.complete(),
    transaction(scope) {
      const queue = []
      let aborted = false
      const tx = {
        error: null,
        oncomplete: null,
        onabort: null,
        abort() {
          aborted = true
        },
        objectStore(name) {
          const request = action => {
            const req = { result: undefined, onsuccess: null }
            queue.push(() => {
              req.result = action()
              req.onsuccess?.()
            })
            return req
          }
          return {
            get: key => request(() => clone(local.get(name).get(JSON.stringify(key)))),
            put: row =>
              request(() => {
                local.get(name).set(keyFor(name, row), clone(row))
              }),
            add: row =>
              request(() => {
                const key = keyFor(name, row)
                if (local.get(name).has(key)) {
                  aborted = true
                  tx.error = new Error('constraint')
                } else local.get(name).set(key, clone(row))
              }),
            delete: key =>
              request(() => {
                local.get(name).delete(JSON.stringify(key))
              })
          }
        }
      }
      let local
      tail = tail.then(
        () =>
          new Promise(resolve => {
            local = new Map(
              scope.map(name => [name, new Map([...stores.get(name)].map(([key, value]) => [key, clone(value)]))])
            )
            while (queue.length && !aborted) queue.shift()()
            const finish = () => {
              if (failure) {
                tx.error = failure
                failure = undefined
                aborted = true
              }
              if (aborted) tx.onabort?.()
              else {
                for (const [name, rows] of local) stores.set(name, rows)
                tx.oncomplete?.()
              }
              resolve()
            }
            if (hold && !hold.complete) {
              hold.complete = finish
              hold.processed()
            } else finish()
          })
      )
      return tx
    }
  }
}

async function reserveJournal(database, declaration) {
  const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
  const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
  const reservations = createWebAuthorityReservations(database)
  const { ownerEpoch: _epoch, ...input } = declaration
  const { owner } = await reservations.acquire({
    declaration: input,
    ownerToken: 'a'.repeat(64),
    operationId: 'b'.repeat(64),
    expectedGeneration: null,
    coordinator: { kind: 'locks', name: 'test' }
  })
  const journal = createWebOrderedRecordingJournal(database, { ...input, ownerEpoch: owner.ownerEpoch }, owner)
  return {
    ...journal,
    bindPeer: peer =>
      require('../../src/web/web-recording-peer-identity')
        .createWebRecordingPeerIdentityStore(database)
        .bindOwned(owner, input.producerIncarnation, peer),
    open: () => journal.open('c'.repeat(64)),
    seal: async () => {
      await reservations.cleanupPending(owner, 'd'.repeat(64))
      await journal.seal()
    },
    anotherInstance: () =>
      createWebOrderedRecordingJournal(database, { ...input, ownerEpoch: owner.ownerEpoch }, owner),
    releaseReservation: () => reservations.release(owner, 'e'.repeat(64))
  }
}

function recordingBridge(database, declaration) {
  const { createWebRecordingJournalBridge } = require('../../src/web/web-recording-journal-bridge')
  let journal
  return createWebRecordingJournalBridge(
    database,
    async memberEpochs => {
      journal = await reserveJournal(database, { ...declaration, memberEpochs })
      return declaration
    },
    async () => ({ current: () => true, release: async () => {} }),
    () => journal
  )
}

module.exports = { recordingDatabase, reserveJournal, recordingBridge }
