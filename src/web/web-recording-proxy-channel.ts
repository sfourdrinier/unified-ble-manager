import type { WebRecordingProxyChannel } from './web-recording-proxy'

/** The common subset of BroadcastChannel and an explicitly transferred MessagePort.
 * The caller selects a partition-local BroadcastChannel name, or transfers a port
 * through an existing trusted window relationship before constructing the adapter. */
export interface WebRecordingNativeMessageChannel {
  postMessage(message: string): void
  addEventListener(type: 'message', listener: (event: { readonly data: unknown }) => void): void
  removeEventListener(type: 'message', listener: (event: { readonly data: unknown }) => void): void
  start?(): void
  close(): void
}
export const WEB_RECORDING_RENDEZVOUS_CHANNEL = 'ubm-web-recording-rendezvous-v1'
export function createWebRecordingProxyChannel(port: WebRecordingNativeMessageChannel): WebRecordingProxyChannel {
  const listeners = new Set<(message: unknown) => void>()
  let closed = false
  const receive = (event: { readonly data: unknown }): void => {
    for (const listener of listeners) listener(event.data)
  }
  port.addEventListener('message', receive)
  port.start?.()
  return {
    postMessage(message) {
      if (closed) throw new Error('web recording channel closed')
      port.postMessage(message)
    },
    subscribe(listener) {
      if (closed) throw new Error('web recording channel closed')
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
    close() {
      if (closed) return
      closed = true
      listeners.clear()
      port.removeEventListener('message', receive)
      port.close()
    }
  }
}
