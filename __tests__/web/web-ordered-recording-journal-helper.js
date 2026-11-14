// A transactional request queue: mutations become visible only on completion.
function transactionalDatabase() {
  const stores = new Map(
    [
      'recordings',
      'records',
      'pendingIdentities',
      'readerReceipts',
      'authorityReservations',
      'markerLedger',
      'markerOperations',
      'recoveryOperations',
      'abandonments',
      'terminalEvidence',
      'terminalEvidenceState'
    ].map(name => [name, new Map()])
  )
  stores.get('readerReceipts').set(JSON.stringify(':ubm-authority-policy-v1'), {
    recordingId: ':ubm-authority-policy-v1',
    version: 2,
    protocol: 'ubm-authority-reservations/1'
  })
  const key = (name, row) =>
    JSON.stringify(
      name === 'recordings' || name === 'authorityReservations'
        ? row.id
        : name === 'records'
          ? [row.recordingId, row.ordinal]
          : name === 'readerReceipts'
            ? row.recordingId
            : name === 'terminalEvidence'
              ? row.ref
              : name === 'terminalEvidenceState'
                ? row.key
                : name === 'markerLedger'
                  ? [row.recordingId, row.producerIncarnation, row.markerId]
                  : name === 'markerOperations' || name === 'recoveryOperations'
                    ? [row.recordingId, row.producerIncarnation, row.operationId]
                    : name === 'abandonments'
                      ? [row.recordingId, row.producerIncarnation]
                      : [
                          row.recordingId,
                          row.producerIncarnation,
                          row.collectionId,
                          row.sourceGeneration,
                          row.deliveryId
                        ]
    )
  const clone = x => (x === undefined ? undefined : JSON.parse(JSON.stringify(x)))
  let paused = false,
    resume,
    processed,
    abortError
  const transactions = []
  let active = false
  const startNext = () => {
    if (active || transactions.length === 0) return
    active = true
    queueMicrotask(transactions.shift())
  }
  return {
    rows: name => [...stores.get(name).values()].map(clone),
    mutate: (name, fn) => {
      for (const row of stores.get(name).values()) fn(row)
    },
    pauseNextCompletion: () => {
      paused = true
      processed = new Promise(resolve => {
        resume = { processed: resolve }
      })
    },
    requestsProcessed: () => processed,
    complete: () => resume.complete(),
    remove: name => stores.get(name).clear(),
    modes: [],
    fallbackDurability: false,
    abortNext: error => {
      abortError = error
    },
    abortAfter: (successfulTransactions, error) => {
      abortError = { after: successfulTransactions, error }
    },
    transaction(names, mode, options) {
      if (this.fallbackDurability && options) throw new TypeError('durability unsupported')
      this.modes.push(mode)
      let local
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
          const request = fn => {
            const req = { result: undefined, error: null, onsuccess: null }
            queue.push(() => {
              req.result = fn()
              req.onsuccess?.()
            })
            return req
          }
          return {
            get: k => request(() => clone(local.get(name).get(JSON.stringify(k)))),
            count: range =>
              request(
                () =>
                  [...local.get(name).values()].filter(
                    row => row.recordingId >= range.lower && row.recordingId <= range.upper
                  ).length
              ),
            getAll: (range, count) =>
              request(() => {
                if (name === 'terminalEvidence' && range === undefined)
                  return [...local.get(name).values()].slice(0, count).map(clone)
                const lower = range.lower
                const id = Array.isArray(lower) ? lower[0] : range.id
                const incarnation = Array.isArray(lower) ? lower[1] : range.incarnation
                const after = Array.isArray(lower) && range.lowerOpen ? lower[2] : range.after
                const field = name === 'markerOperations' ? 'operationId' : 'markerId'
                return [...local.get(name).values()]
                  .filter(row =>
                    typeof lower === 'string'
                      ? row.recordingId >= lower &&
                        row.recordingId <= range.upper &&
                        (!range.lowerOpen || row.recordingId > lower)
                      : row.recordingId === id &&
                        row.producerIncarnation === incarnation &&
                        (after === undefined || row[field] > after)
                  )
                  .sort((a, b) => {
                    const x = typeof lower === 'string' ? a.recordingId : a[field]
                    const y = typeof lower === 'string' ? b.recordingId : b[field]
                    return x < y ? -1 : x > y ? 1 : 0
                  })
                  .slice(0, count)
                  .map(clone)
              }),
            put: row =>
              request(() => {
                local.get(name).set(key(name, row), clone(row))
              }),
            add: row =>
              request(() => {
                const store = local.get(name)
                const k = key(name, row)
                if (store.has(k)) {
                  tx.error = new Error('ConstraintError')
                  aborted = true
                } else store.set(k, clone(row))
              }),
            delete: k =>
              request(() => {
                local.get(name).delete(JSON.stringify(k))
              })
          }
        }
      }
      transactions.push(() => {
        local = new Map(names.map(name => [name, new Map([...stores.get(name)].map(([k, v]) => [k, clone(v)]))]))
        while (queue.length && !aborted) queue.shift()()
        const finish = () => {
          if (abortError) {
            if (typeof abortError === 'object' && abortError.after !== undefined && abortError.after > 0) {
              abortError.after -= 1
            } else {
              tx.error =
                typeof abortError === 'object' && abortError.error !== undefined ? abortError.error : abortError
              abortError = undefined
              aborted = true
            }
          }
          if (aborted) tx.onabort?.()
          else {
            for (const [name, rows] of local) stores.set(name, rows)
            tx.oncomplete?.()
          }
          active = false
          startNext()
        }
        if (paused) {
          paused = false
          resume.complete = finish
          resume.processed()
        } else finish()
      })
      startNext()
      return tx
    }
  }
}
module.exports = { transactionalDatabase }
