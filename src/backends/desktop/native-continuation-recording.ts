import { contractError } from '../../backend-contract/errors'
import {
  createNativeContinuationRecordingController,
  type ContinuationRecordingController
} from '../../core/continuation-recording'
import { rehydratePublicPromise } from '../../public/error-bridge'
import type { DesktopRustCoreBinding } from './desktop-rust-core-binding'
import { nativeContinuationTransportError } from '../../core/native-continuation-envelope'

/** Trusted Node/Electron main only. The application selects its private data
 * directory; never forward arbitrary renderer paths. This verifies/opens the
 * store through the loaded binding without creating or enumerating a radio. */
export function openNativeContinuationRecordings(
  binding: DesktopRustCoreBinding,
  directory: string
): Promise<ContinuationRecordingController> {
  return rehydratePublicPromise(
    (async () => {
      if (typeof directory !== 'string' || directory.length === 0) {
        throw contractError('argument.invalid', 'restoration', 'continuation.recording.directory')
      }
      if (typeof binding.openRecordingStore !== 'function') {
        throw contractError('capability.unsupported', 'restoration', 'continuation.recording.open')
      }
      const open = binding.openRecordingStore
      const store = await Promise.resolve()
        .then(() => open.call(binding, directory))
        .catch(error => {
          throw nativeContinuationTransportError(error, 'ubm-desktop', 'continuation.recording.open')
        })
      return createNativeContinuationRecordingController(store, 'ubm-desktop')
    })()
  )
}
