import type { ScanStateEvent } from './ble-manager'

export interface ScanStateController {
  readonly stream: AsyncIterable<ScanStateEvent>
  emit(event: ScanStateEvent): void
  close(): void
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
