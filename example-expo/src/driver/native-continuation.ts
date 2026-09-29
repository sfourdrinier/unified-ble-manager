import type { createNativeContinuationControl } from 'unified-ble-manager/backend-sdk'
import type { ContinuationRecordingController } from 'unified-ble-manager'
import type { TurboModule } from 'react-native'

/** App-only transport; production UnifiedBleRustCore's API is unchanged. */
export interface ReferenceNativeContinuationModule extends TurboModule {
  invoke(operation: string, peer: string, declarationJson: string, token: string,
    maxItems: number, maxBytes: number): Promise<string>
}

export function createReferenceNativeContinuation(
  native: ReferenceNativeContinuationModule,
  verifyIdentity: () => Promise<void>,
  createControl: typeof createNativeContinuationControl,
  recordings: () => ContinuationRecordingController
) {
  const invoke = async (operation: string, peer = '', declaration = '', token = '', items = 0, bytes = 0) => {
    await verifyIdentity()
    return native.invoke(operation, peer, declaration, token, items, bytes)
  }
  return Object.freeze({
    ...createControl({
      execute: (peer, declaration) => invoke('execute', peer, declaration),
      describeBacklog: () => invoke('status'),
      prepareClaim: (items, bytes) => invoke('prepare', '', '', '', items, bytes),
      acknowledgeClaim: token => invoke('acknowledge', '', '', token)
    }, { hostDomain: 'ubm-mobile', scope: 'react-native-native', format: 'mobile' }),
    recordings: async () => recordings()
  })
}
