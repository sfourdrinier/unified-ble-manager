import { webRecordingContractError as contractError, webStorageErrorName } from './web-recording-errors'
import { ownWebIndexedDbTransaction } from './web-indexeddb-transaction'
import type { WebIndexedDbDatabase, WebIndexedDbSettlement, WebIndexedDbTransaction } from './web-indexeddb-transaction'
import type { WebRecordingLockManager } from './web-recording-lock'

interface AdmissionLocks {
  request(
    name: string,
    options: { readonly mode: 'exclusive' | 'shared'; readonly ifAvailable: true },
    callback: (lock: object | null) => Promise<void> | void
  ): Promise<void>
  query(): Promise<{ readonly held?: readonly { readonly name?: string; readonly mode?: string }[] }>
}
interface Lease {
  release(): void
  readonly ended: Promise<void>
}
const requests = new WeakMap<object, Set<string>>()
const unavailable = (detail = 'clear-admission-uncertain'): Error =>
  contractError('capability.unavailable', 'restoration', `web-recording.${detail}`)
const unsupported = (): Error =>
  contractError('capability.unsupported', 'restoration', 'web-recording.clear-admission-locks')
const lockError = (error: unknown): Error => {
  const name = webStorageErrorName(error)
  return contractError(
    name === 'SecurityError' || name === 'NotSupportedError' ? 'capability.unsupported' : 'capability.unavailable',
    'restoration',
    'web-recording.clear-admission-locks',
    { domain: 'Web Locks', code: name, safeMessage: 'Web Locks admission coordination failed', metadata: {} }
  )
}

function admissionLocks(locks: WebRecordingLockManager | undefined): AdmissionLocks {
  if (locks === undefined || !('query' in locks) || typeof locks.query !== 'function') throw unsupported()
  // WebRecordingLockManager is the narrower common interface used by the other
  // lock owners; this coordinator additionally requires the standard query and
  // shared-lock operations.
  return {
    request: (name, options, callback) => {
      if (options.mode === 'exclusive') return locks.request(name, { mode: 'exclusive', ifAvailable: true }, callback)
      const request = Reflect.get(locks, 'request')
      if (typeof request !== 'function') throw unsupported()
      return Reflect.apply(request, locks, [name, options, callback])
    },
    query: () => {
      const query = Reflect.get(locks, 'query')
      if (typeof query !== 'function') throw unsupported()
      return Reflect.apply(query, locks, [])
    }
  }
}

/** One unresolved browser request per lock name and realm, including late callbacks. */
function tryLease(
  locks: AdmissionLocks,
  owner: object,
  name: string,
  mode: 'exclusive' | 'shared',
  abandoned: () => boolean
): Promise<Lease | null> {
  const pending = requests.get(owner) ?? new Set<string>()
  if (pending.has(name)) return Promise.reject(unavailable('clear-admission-lock-pending'))
  pending.add(name)
  requests.set(owner, pending)
  const remove = (): void => {
    pending.delete(name)
    if (pending.size === 0) requests.delete(owner)
  }
  return new Promise((resolve, reject) => {
    let answered = false
    let releaseHold: (() => void) | undefined
    let releaseRequested = false
    let finishEnded: () => void = () => undefined
    let failEnded: (error: unknown) => void = () => undefined
    const ended = new Promise<void>((done, fail) => {
      finishEnded = done
      failEnded = fail
    })
    // An owner may finish after its caller has timed out. Its error is observed
    // here as well as by any still-waiting caller.
    ended.catch(() => undefined)
    let request: Promise<void>
    try {
      request = locks.request(name, { mode, ifAvailable: true }, lock => {
        answered = true
        remove()
        if (lock === null || abandoned()) {
          resolve(null)
          return
        }
        const hold = new Promise<void>(done => {
          releaseHold = done
        })
        resolve({
          release: () => {
            releaseRequested = true
            releaseHold?.()
          },
          ended
        })
        return hold
      })
    } catch (error) {
      remove()
      reject(lockError(error))
      return
    }
    request.then(
      () => {
        if (!answered) {
          remove()
          reject(unavailable())
        } else if (releaseRequested) finishEnded()
        else failEnded(unavailable('clear-admission-lock-lost'))
      },
      error => {
        remove()
        if (!answered) reject(lockError(error))
        else failEnded(lockError(error))
      }
    )
  })
}

/** Cross-realm slots are retained through actual settlement, not caller timeout.
 * Each new holder first runs an overlapping readwrite transaction: its commit
 * fences transactions from a former holder whose realm disappeared. */
export function boundedWebClearAdmission<T, TTransaction extends WebIndexedDbTransaction>(
  partition: string,
  suppliedLocks: WebRecordingLockManager | undefined,
  database: WebIndexedDbDatabase<TTransaction>,
  limit: number,
  operation: (
    own: (settled: Promise<WebIndexedDbSettlement>) => void,
    validate: () => void,
    database: WebIndexedDbDatabase<TTransaction>
  ) => Promise<T>
): Promise<T> {
  if (!Number.isSafeInteger(limit) || limit < 1)
    return Promise.reject(contractError('argument.invalid', 'restoration', 'web-recording.clear-admission-limit'))
  let locks: AdmissionLocks
  try {
    locks = admissionLocks(suppliedLocks)
  } catch (error) {
    return Promise.reject(error)
  }
  if (suppliedLocks === undefined) return Promise.reject(unsupported())
  const owner = suppliedLocks
  const prefix = `ubm-ordered-recordings:clear-admission-v1:${JSON.stringify(partition)}:`
  const policyPrefix = `${prefix}policy:`
  let abandoned = false
  let timer: ReturnType<typeof setTimeout> | undefined
  const work = (async (): Promise<T> => {
    const gate = await tryLease(locks, owner, `${prefix}gate`, 'exclusive', () => abandoned)
    if (gate === null) throw unavailable('clear-admission-busy')
    let policy: Lease | null = null
    let slot: Lease | null = null
    let gateLost = false
    gate.ended.then(
      () => {
        gateLost = true
      },
      () => {
        gateLost = true
      }
    )
    const guarded = <V>(value: Promise<V>): Promise<V> =>
      Promise.race([
        value,
        gate.ended.then(() => {
          throw unavailable('clear-admission-lock-lost')
        })
      ])
    const acquire = async (name: string, mode: 'exclusive' | 'shared'): Promise<Lease | null> => {
      const request = tryLease(locks, owner, name, mode, () => abandoned || gateLost)
      try {
        return await guarded(request)
      } catch (error) {
        request.then(
          lease => lease?.release(),
          () => undefined
        )
        throw error
      }
    }
    try {
      const snapshot = await guarded(
        Promise.resolve()
          .then(() => locks.query())
          .catch(error => {
            throw lockError(error)
          })
      )
      if (!Array.isArray(snapshot.held)) throw unavailable('clear-admission-policy')
      if (
        snapshot.held.some(
          entry =>
            typeof entry.name === 'string' &&
            entry.name.startsWith(policyPrefix) &&
            entry.name !== `${policyPrefix}${limit}`
        )
      )
        throw unavailable('clear-admission-policy-conflict')
      if (abandoned) throw unavailable()
      policy = await acquire(`${policyPrefix}${limit}`, 'shared')
      if (policy === null) throw unavailable('clear-admission-busy')
      for (let index = 0; index < limit && !abandoned; index++) {
        slot = await acquire(`${prefix}slot:${index}`, 'exclusive')
        if (slot !== null) break
        // Do not starve the caller's deadline while probing a large pool.
        if ((index + 1) % 64 === 0) await guarded(new Promise<void>(resolve => setTimeout(resolve, 0)))
      }
      if (slot === null || abandoned) throw unavailable('clear-admission-busy')
    } catch (error) {
      policy?.release()
      slot?.release()
      throw error
    } finally {
      gate.release()
    }
    const selectedPolicy = policy
    const selectedSlot = slot
    let lost = false
    const markLost = (): void => {
      lost = true
    }
    selectedSlot.ended.then(markLost, markLost)
    selectedPolicy.ended.then(markLost, markLost)
    const validate = (): void => {
      if (lost) throw unavailable('clear-admission-lock-lost')
    }
    let owned = false
    let done = false
    let settled = false
    const release = (): void => {
      if (done && (!owned || settled)) {
        selectedSlot.release()
        selectedPolicy.release()
      }
    }
    try {
      // A readwrite transaction in the same store cannot complete before an
      // earlier admission transaction, even if its JS owner has disappeared.
      const validateDispatch = (): void => {
        validate()
        if (abandoned) throw unavailable()
      }
      const guardedDatabase: WebIndexedDbDatabase<TTransaction> = {
        ready: async () => {
          await database.ready?.()
          validateDispatch()
        },
        transaction: (stores, mode, options) => {
          validateDispatch()
          return database.transaction(stores, mode, options)
        }
      }
      const fence = ownWebIndexedDbTransaction(guardedDatabase, ['readerReceipts'], 'readwrite', () => undefined)
      owned = true
      fence.settled.then(() => {
        settled = true
        release()
      })
      await Promise.race([
        fence.result,
        selectedSlot.ended.then(() => {
          throw unavailable('clear-admission-lock-lost')
        }),
        selectedPolicy.ended.then(() => {
          throw unavailable('clear-admission-lock-lost')
        })
      ])
      if (abandoned) throw unavailable()
      validate()
      owned = false
      settled = false
      const result = operation(
        actual => {
          owned = true
          actual.then(() => {
            settled = true
            release()
          })
        },
        validate,
        guardedDatabase
      )
      return await Promise.race([
        result,
        selectedSlot.ended.then(() => {
          throw unavailable('clear-admission-lock-lost')
        }),
        selectedPolicy.ended.then(() => {
          throw unavailable('clear-admission-lock-lost')
        })
      ])
    } finally {
      done = true
      release()
    }
  })()
  return Promise.race([
    work,
    new Promise<never>((_resolve, reject) => {
      timer = setTimeout(() => {
        abandoned = true
        reject(unavailable())
      }, 5000)
    })
  ]).finally(() => {
    if (timer !== undefined) clearTimeout(timer)
  })
}
