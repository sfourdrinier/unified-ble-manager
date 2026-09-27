import { Platform } from 'react-native'
import { rehydratePublicError } from './public/error-bridge'
import { contractError } from './backend-contract/errors'
import { createReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core-binding'
import type { ReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core'
import {
  createNativeContinuationRecordingController,
  type ContinuationRecordingController
} from './core/continuation-recording'

/** Opens no BLE session and requests no Bluetooth permission. Native chooses
 * protected app-private storage; callers supply only recording IDs, not paths.
 * The production binding verifies the sealed binary identity before each call. */
export function createReactNativeContinuationRecordings(
  options: { readonly rustCore?: ReactNativeRustCoreBinding } = {}
): ContinuationRecordingController {
  let binding: ReactNativeRustCoreBinding
  try {
    binding = options.rustCore ?? defaultBinding()
  } catch (error) {
    throw rehydratePublicError(error)
  }
  const missing = () =>
    Promise.reject(contractError('capability.unsupported', 'restoration', 'continuation.recording.native-controls'))
  return createNativeContinuationRecordingController(
    {
      status: id =>
        typeof binding.continuationRecordingStatus === 'function' ? binding.continuationRecordingStatus(id) : missing(),
      prepare: (id, maxItems, maxBytes) =>
        typeof binding.continuationRecordingPrepare === 'function'
          ? binding.continuationRecordingPrepare(id, maxItems, maxBytes)
          : missing(),
      acknowledge: (id, token) =>
        typeof binding.continuationRecordingAcknowledge === 'function'
          ? binding.continuationRecordingAcknowledge(id, token)
          : missing(),
      stop: id =>
        typeof binding.continuationRecordingStop === 'function' ? binding.continuationRecordingStop(id) : missing(),
      clear: id =>
        typeof binding.continuationRecordingClear === 'function' ? binding.continuationRecordingClear(id) : missing()
    },
    'ubm-mobile'
  )
}

function defaultBinding(): ReactNativeRustCoreBinding {
  if (Platform.OS !== 'android' && Platform.OS !== 'ios') {
    throw contractError('capability.unsupported', 'platform', 'continuation.recording.mobile-host')
  }
  return createReactNativeRustCoreBinding({ platform: Platform.OS === 'android' ? 'android' : 'apple' })
}
