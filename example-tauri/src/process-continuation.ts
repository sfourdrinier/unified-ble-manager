import { createNativeContinuationControl, createNativeContinuationRecordingController } from 'unified-ble-manager/tauri'
import type { HostNativeContinuation } from '../../examples-shared/driver/host.ts'

/** App-scoped transport, not the plugin BLE bootstrap. Host owns authentication. */
export function createTauriProcessContinuation(
  invoke: (
    command: string,
    args: { readonly request: { readonly operation: string; readonly args: Readonly<Record<string, string | number>> } }
  ) => Promise<unknown>
): HostNativeContinuation {
  const call = (operation: string, args: Readonly<Record<string, string | number>> = {}) =>
    invoke('reference_process_continuation', { request: { operation, args } })
  const control = createNativeContinuationControl({
    execute: (peerId, declarationJson) => call('execute', { peerId, declarationJson }),
    describeBacklog: () => call('status'),
    prepareClaim: (maxItems, maxBytes) => call('prepare-claim', { maxItems, maxBytes }),
    acknowledgeClaim: token => call('acknowledge-claim', { token })
  })
  const recordings = createNativeContinuationRecordingController(
    {
      status: id => call('recording-status', { id }),
      prepare: (id, maxItems, maxBytes) => call('recording-prepare', { id, maxItems, maxBytes }),
      acknowledge: (id, token) => call('recording-acknowledge', { id, token }),
      stop: id => call('recording-stop', { id }),
      clear: id => call('recording-clear', { id })
    },
    'tauri-reference'
  )
  return Object.freeze({ ...control, recordings: async () => recordings })
}
