// Deterministic transaction scheduler: overlapping transactions serialize and aborts roll back.
function recordingDatabase() {
  const names = ['recordings', 'records', 'pendingIdentities', 'readerReceipts', 'authorityReservations', 'markerLedger', 'markerOperations', 'recoveryOperations', 'abandonments']
  const stores = new Map(names.map(name => [name, new Map()]))
  stores.get('readerReceipts').set(JSON.stringify(':ubm-authority-policy-v1'), {
    recordingId: ':ubm-authority-policy-v1', version: 2, protocol: 'ubm-authority-reservations/1'
  })
  const clone = value => (value === undefined ? undefined : structuredClone(value))
  const keyFor = (name, row) =>
    JSON.stringify(
      name === 'recordings' || name === 'authorityReservations'
        ? row.id
        : name === 'markerLedger'
          ? [row.recordingId, row.producerIncarnation, row.markerId]
          : name === 'markerOperations' || name === 'recoveryOperations'
            ? [row.recordingId, row.producerIncarnation, row.operationId]
        : name === 'abandonments'
              ? [row.recordingId, row.producerIncarnation]
        : name === 'records'
          ? [row.recordingId, row.ordinal]
          : name === 'readerReceipts'
            ? row.recordingId
            : [row.recordingId, row.producerIncarnation, row.collectionId, row.sourceGeneration, row.deliveryId]
    )
  let tail = Promise.resolve(),
    failure,
    hold,
    processed,
    suppressCompletion = false
  return {
    rows: name => [...stores.get(name).values()].map(clone),
    seed: (name, row) => stores.get(name).set(keyFor(name, row), clone(row)),
    mutate: (name, fn) => {
      for (const row of stores.get(name).values()) fn(row)
    },
    remove: name => stores.get(name).clear(),
    abortNext: error => {
      failure = error
    },
    suppressNextCompletion: () => {
      suppressCompletion = true
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
            count: range => request(() => [...local.get(name).values()].filter(row =>
              row.recordingId >= range.lower && row.recordingId <= range.upper).length),
            getAll: (range, count) => request(() => {
              const lower = range.lower
              const id = Array.isArray(lower) ? lower[0] : range.id
              const incarnation = Array.isArray(lower) ? lower[1] : range.incarnation
              const after = Array.isArray(lower) && range.lowerOpen ? lower[2] : range.after
              const field = name === 'markerOperations' ? 'operationId' : 'markerId'
              return [...local.get(name).values()].filter(row => typeof lower === 'string'
                ? row.recordingId >= lower && row.recordingId <= range.upper && (!range.lowerOpen || row.recordingId > lower)
                : row.recordingId === id && (incarnation === undefined || row.producerIncarnation === incarnation) && (after === undefined || row[field] > after))
                .sort((a, b) => {
                  const x = typeof lower === 'string' ? a.recordingId : a[field]
                  const y = typeof lower === 'string' ? b.recordingId : b[field]
                  return x < y ? -1 : x > y ? 1 : 0
                }).slice(0, count).map(clone)
            }),
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
                if (suppressCompletion) suppressCompletion = false
                else tx.oncomplete?.()
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
module.exports = { recordingDatabase }
