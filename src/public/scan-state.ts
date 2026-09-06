import type { NormalizedBleError } from '../backend-contract/errors'
import type { StreamTerminalNotice } from '../backend-contract/streams'
import type { ScanStateEvent } from './ble-manager'

type ScanSourceTerminalError = NormalizedBleError | null | undefined
type ScanSourceTerminalObserver = (reason: StreamTerminalNotice['reason'], error?: ScanSourceTerminalError) => void

export interface ScanStateController {
  readonly stream: AsyncIterable<ScanStateEvent>
  emit(event: ScanStateEvent): void
  close(): void
}

/**
 * Projects a source/host terminal into public scan state.
 *
 * This is ended delivery, not physical cleanup: `source-failed`,
 * `connection-lost`, and `overflow` become `failed`; ordinary close becomes
 * `stopped`. `stop()` still reports remaining native/session cleanup.
 */
export function projectScanDeliveryTerminal(reason: StreamTerminalNotice['reason']): ScanStateEvent {
  if (reason === 'source-failed' || reason === 'connection-lost' || reason === 'overflow') {
    return Object.freeze({ state: 'failed', reason })
  }
  return Object.freeze({ state: 'stopped', reason })
}

/** Observes producer-side terminal methods so state updates without an iterator. */
export function bindScanSourceTerminal(source: object, onTerminal: ScanSourceTerminalObserver): void {
  bindScanSourceTerminalMethod(source, 'finishWithReason', onTerminal)
  bindScanSourceTerminalMethod(source, 'closeWithReason', onTerminal)
  const already = readScanSourceTerminalReason(source)
  if (already !== null) onTerminal(already)
}

function readScanSourceTerminalReason(source: object): StreamTerminalNotice['reason'] | null {
  const reader = Reflect.get(source, 'terminalReason')
  if (typeof reader !== 'function') return null
  const reason = reader.call(source)
  return isStreamTerminalReason(reason) ? reason : null
}

function isStreamTerminalReason(value: unknown): value is StreamTerminalNotice['reason'] {
  return (
    value === 'closed' ||
    value === 'overflow' ||
    value === 'source-failed' ||
    value === 'owner-released' ||
    value === 'connection-lost' ||
    value === 'service-changed' ||
    value === 'operation-aborted' ||
    value === 'operation-timed-out'
  )
}

function bindScanSourceTerminalMethod(
  source: object,
  methodName: 'finishWithReason' | 'closeWithReason',
  onTerminal: ScanSourceTerminalObserver
): void {
  const method = Reflect.get(source, methodName)
  if (typeof method !== 'function') return
  Reflect.set(source, methodName, (reason: StreamTerminalNotice['reason'], error?: ScanSourceTerminalError) => {
    method.call(source, reason, error)
    onTerminal(reason, error)
  })
}

interface StateWaiter {
  readonly resolve: (result: IteratorResult<ScanStateEvent, undefined>) => void
}

export function createScanState(): ScanStateController {
  const queued: ScanStateEvent[] = []
  const waiters: StateWaiter[] = []
  let closed = false
  const emit = (event: ScanStateEvent): void => {
    if (closed) return
    const waiter = waiters.shift()
    if (waiter !== undefined) {
      waiter.resolve({ done: false, value: Object.freeze({ ...event }) })
      return
    }
    queued.push(Object.freeze({ ...event }))
  }
  const close = (): void => {
    if (closed) return
    closed = true
    while (true) {
      const waiter = waiters.shift()
      if (waiter === undefined) return
      waiter.resolve({ done: true, value: undefined })
    }
  }
  return {
    stream: {
      [Symbol.asyncIterator]() {
        return {
          next: async (): Promise<IteratorResult<ScanStateEvent, undefined>> => {
            const next = queued.shift()
            if (next !== undefined) return { done: false, value: next }
            if (closed) return { done: true, value: undefined }
            return new Promise(resolve => waiters.push({ resolve }))
          },
          [Symbol.asyncIterator]() {
            return this
          }
        }
      }
    },
    emit,
    close
  }
}
