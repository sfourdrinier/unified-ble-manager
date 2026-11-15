import { webRecordingContractError as contractError, webStorageErrorName } from './web-recording-errors'
import type { WebRecordingLockManager } from './web-recording-lock'

// An ifAvailable request does not queue behind an owner. Still retain one local
// request until its callback settles if the browser fails to deliver it.
const requests = new WeakMap<WebRecordingLockManager, Set<string>>()
const unavailable = (detail: string): Error =>
  contractError('capability.unavailable', 'restoration', `web-recording.${detail}`)

export type WebRecordingClearLock = (() => void) & {
  readonly ended: Promise<{ readonly state: 'released' } | { readonly state: 'lost'; readonly error: unknown }>
}

function establishmentError(error: unknown): Error {
  const name = webStorageErrorName(error)
  return contractError(
    name === 'SecurityError' || name === 'NotSupportedError' ? 'capability.unsupported' : 'capability.unavailable',
    'restoration',
    'web-recording.clear-web-lock',
    { domain: 'Web Locks', code: name, safeMessage: 'Web Locks did not establish authority', metadata: {} }
  )
}

/** One clear owner per IndexedDB journal in this browser storage partition. */
export function acquireWebRecordingClearLock(
  locks: WebRecordingLockManager | undefined,
  onLost?: (error: unknown) => void,
  journalIdentity = 'ubm-ordered-recordings',
  signal?: AbortSignal
): Promise<WebRecordingClearLock> {
  if (signal?.aborted)
    return Promise.reject(contractError('operation.aborted', 'restoration', 'web-recording.clear-attempt'))
  if (locks === undefined)
    return Promise.reject(contractError('capability.unsupported', 'restoration', 'web-recording.clear-web-lock'))
  const name = `ubm-ordered-recordings:clear-v2:${JSON.stringify(journalIdentity)}`
  const pending = requests.get(locks) ?? new Set<string>()
  if (pending.has(name)) return Promise.reject(unavailable('clear-lock-pending'))
  pending.add(name)
  requests.set(locks, pending)
  const removeRequest = (): void => {
    pending.delete(name)
    if (pending.size === 0) requests.delete(locks)
  }
  return new Promise((resolve, reject) => {
    let abandoned = false
    let answered = false
    let granted = false
    let releasing = false
    let finishEnded: (result: { state: 'released' } | { state: 'lost'; error: unknown }) => void
    const ended = new Promise<{ state: 'released' } | { state: 'lost'; error: unknown }>(finish => {
      finishEnded = finish
    })
    const timer = setTimeout(() => {
      abandoned = true
      signal?.removeEventListener('abort', abort)
      reject(unavailable('clear-lock-uncertain'))
    }, 5000)
    const abort = (): void => {
      if (answered || abandoned) return
      abandoned = true
      clearTimeout(timer)
      signal?.removeEventListener('abort', abort)
      reject(contractError('operation.aborted', 'restoration', 'web-recording.clear-attempt'))
    }
    signal?.addEventListener('abort', abort, { once: true })
    if (signal?.aborted) abort()
    let request: Promise<void>
    try {
      request = locks.request(name, { mode: 'exclusive', ifAvailable: true }, lock => {
        answered = true
        removeRequest()
        clearTimeout(timer)
        signal?.removeEventListener('abort', abort)
        if (abandoned) return
        if (lock === null) {
          reject(unavailable('clear-busy'))
          return
        }
        granted = true
        return new Promise<void>(release => {
          const lease: WebRecordingClearLock = Object.assign(
            () => {
              releasing = true
              release()
            },
            { ended }
          )
          resolve(lease)
        })
      })
    } catch (error) {
      removeRequest()
      clearTimeout(timer)
      signal?.removeEventListener('abort', abort)
      reject(establishmentError(error))
      return
    }
    const lost = (error: unknown): void => {
      let problem = error
      try {
        onLost?.(error)
      } catch (callbackError) {
        problem = new AggregateError([error, callbackError], 'Clear lock loss callback failed')
      }
      finishEnded({ state: 'lost', error: problem })
    }
    request.then(
      () => {
        if (granted) {
          if (releasing) finishEnded({ state: 'released' })
          else lost(null)
        } else if (!answered) {
          removeRequest()
          clearTimeout(timer)
          signal?.removeEventListener('abort', abort)
          if (!abandoned) reject(unavailable('clear-lock-uncertain'))
        }
      },
      error => {
        removeRequest()
        clearTimeout(timer)
        signal?.removeEventListener('abort', abort)
        if (granted) lost(error)
        else if (!answered && !abandoned) reject(establishmentError(error))
      }
    )
  })
}
