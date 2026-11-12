import { webRecordingContractError as contractError, webStorageErrorName } from './web-recording-errors'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'

export interface WebRecordingLockManager {
  request(
    name: string,
    options: { readonly mode: 'exclusive'; readonly ifAvailable: true },
    callback: (lock: object | null) => Promise<void> | void
  ): Promise<void>
}

export type WebRecordingLock =
  | { readonly state: 'busy' }
  | {
      readonly state: 'acquired'
      /** Settles if authority ends, including unexpected coordinator failure. */
      readonly ended: Promise<{ readonly state: 'released' } | { readonly state: 'lost'; readonly error: unknown }>
      release(): Promise<void>
    }

/** Abandon a wait without abandoning the underlying transaction or promise. */
export function awaitWebRecordingAbortable<T>(work: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (signal === undefined) return work
  return new Promise((resolve, reject) => {
    const abort = (): void => {
      signal.removeEventListener('abort', abort)
      reject(contractError('operation.aborted', 'restoration', 'web-recording.acquire'))
    }
    if (signal.aborted) {
      work.catch(() => undefined)
      abort()
      return
    }
    signal.addEventListener('abort', abort, { once: true })
    work.then(
      value => {
        signal.removeEventListener('abort', abort)
        resolve(value)
      },
      error => {
        signal.removeEventListener('abort', abort)
        reject(error)
      }
    )
  })
}

/** The digest must be SHA-256 of the ID, encoded as lowercase hexadecimal. */
export async function acquireWebRecordingLock(
  locks: WebRecordingLockManager,
  digest: (recordingId: string) => Promise<string>,
  recordingId: string,
  onLost?: (error: unknown) => void,
  signal?: AbortSignal
): Promise<WebRecordingLock> {
  if (!isRecordingId(recordingId)) throw contractError('argument.invalid', 'restoration', 'web-recording.lock-id')
  const hash = await awaitWebRecordingAbortable(digest(recordingId), signal)
  if (!/^[0-9a-f]{64}$/.test(hash))
    throw contractError('protocol.malformed', 'restoration', 'web-recording.lock-digest')

  return new Promise((resolve, reject) => {
    let granted = false
    let releasing = false
    let abandoned = false
    const aborted = (): Error => contractError('operation.aborted', 'restoration', 'web-recording.acquire')
    const unavailable = (error: unknown): Error =>
      contractError('capability.unsupported', 'restoration', 'web-recording.web-lock-unavailable', {
        domain: 'Web Locks',
        code: webStorageErrorName(error),
        safeMessage: 'Web Locks did not establish authority',
        metadata: {}
      })
    const detachAbort = (): void => signal?.removeEventListener('abort', abort)
    const abort = (): void => {
      if (granted || abandoned) return
      abandoned = true
      detachAbort()
      reject(aborted())
    }
    if (signal?.aborted) {
      reject(aborted())
      return
    }
    signal?.addEventListener('abort', abort, { once: true })
    let releaseHold: (() => void) | null = null
    let resolveEnded: (value: { state: 'released' } | { state: 'lost'; error: unknown }) => void
    const ended = new Promise<{ state: 'released' } | { state: 'lost'; error: unknown }>(done => {
      resolveEnded = done
    })
    let request: Promise<void>
    try {
      request = locks.request(`ubm-ordered-recording:${hash}`, { mode: 'exclusive', ifAvailable: true }, lock => {
        if (abandoned) return
        detachAbort()
        if (lock === null) {
          resolve({ state: 'busy' })
          return
        }
        granted = true
        const hold = new Promise<void>(done => {
          releaseHold = done
        })
        resolve({
          state: 'acquired',
          ended,
          release: async () => {
            if (!releasing) {
              releasing = true
              releaseHold?.()
            }
            await request
          }
        })
        return hold
      })
    } catch (error) {
      detachAbort()
      if (!abandoned) reject(unavailable(error))
      return
    }
    request.then(
      () => {
        if (granted) {
          if (releasing) resolveEnded({ state: 'released' })
          else {
            let problem: unknown = null
            try {
              onLost?.(null)
            } catch (error) {
              problem = error
            }
            resolveEnded({ state: 'lost', error: problem })
          }
        }
      },
      error => {
        if (granted) {
          let problem: unknown = error
          try {
            onLost?.(error)
          } catch (callbackError) {
            problem = new AggregateError([error, callbackError], 'Authority loss callback failed')
          }
          resolveEnded({ state: 'lost', error: problem })
        } else {
          detachAbort()
          if (!abandoned) reject(unavailable(error))
        }
      }
    )
  })
}
