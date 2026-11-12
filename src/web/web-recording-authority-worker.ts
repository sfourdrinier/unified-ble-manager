/** A HELLO is compatibility evidence only. It neither creates nor releases
 * recording authority; durable reservation transactions make that decision. */
export interface WebRecordingCoordinatorPort {
  onmessage: ((event: { readonly data: unknown }) => void) | null
  onmessageerror: (() => void) | null
  onclose?: (() => void) | null
  postMessage(message: unknown): void
  start(): void
  close(): void
}
export interface WebRecordingCoordinatorScope {
  onconnect: ((event: { readonly ports: readonly WebRecordingCoordinatorPort[] }) => void) | null
}

export const WEB_RECORDING_WORKER_VERSION = 2
export const WEB_RECORDING_WORKER_NAME = 'ubm-ordered-recording-v2'
export const WEB_RECORDING_WORKER_HANDSHAKE_TIMEOUT_MS = 5000
const MAX_PENDING_PORTS = 16

export function installWebRecordingAuthorityWorker(scope: WebRecordingCoordinatorScope): void {
  // This count limits setup resources only. It never identifies a recording or owner.
  let pendingPorts = 0
  scope.onconnect = event => {
    const port = event.ports[0]
    if (port === undefined) return
    if (pendingPorts >= MAX_PENDING_PORTS) {
      port.close()
      return
    }
    pendingPorts++
    let finished = false
    const close = (): void => {
      if (finished) return
      finished = true
      clearTimeout(timer)
      pendingPorts--
      port.onmessage = null
      port.onmessageerror = null
      if ('onclose' in port) port.onclose = null
      port.close()
    }
    const timer = setTimeout(close, WEB_RECORDING_WORKER_HANDSHAKE_TIMEOUT_MS)
    port.onmessageerror = close
    if ('onclose' in port) port.onclose = close
    port.onmessage = message => {
      const data = message.data
      try {
        if (
          typeof data === 'object' &&
          data !== null &&
          'type' in data &&
          data.type === 'hello' &&
          'version' in data &&
          data.version === WEB_RECORDING_WORKER_VERSION &&
          'requestId' in data &&
          typeof data.requestId === 'string' &&
          /^[1-9][0-9]{0,31}$/.test(data.requestId)
        ) {
          port.postMessage({ type: 'hello', version: WEB_RECORDING_WORKER_VERSION, requestId: data.requestId })
        }
      } finally {
        close()
      }
    }
    try {
      port.start()
    } catch (error) {
      close()
      throw error
    }
  }
}
