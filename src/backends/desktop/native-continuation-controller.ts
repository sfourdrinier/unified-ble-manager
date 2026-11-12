import { contractError } from '../../backend-contract/errors'
import { assertDesktopOrderedReceiptVersion } from './ordered-recording-version'
import { rehydratePublicPromise } from '../../public/error-bridge'
import type { DesktopRustCoreCentral } from './desktop-rust-core-binding'
import { assertContinuationRecordingConfigured } from '../../core/native-continuation-envelope'
import {
  createNativeContinuationRecordingController,
  type ContinuationRecordingController
} from '../../core/continuation-recording'
import {
  createNativeContinuationControl,
  callNativeContinuationControl as call,
  type NativeContinuationControl,
  type NativeContinuationControlAccess
} from '../../core/native-continuation-control'
export { createNativeContinuationControl } from '../../core/native-continuation-control'
export type {
  NativeContinuationCompleted,
  NativeContinuationFailed,
  NativeContinuationStatus,
  NativeContinuationDesktopStatus,
  NativeContinuationMobileStatus,
  NativeContinuationControl,
  NativeContinuationControlV1,
  NativeContinuationControlAccess,
  NativeContinuationControlContext
} from '../../core/native-continuation-control'

export interface NativeContinuationController extends NativeContinuationControl {
  /** Configure trusted app-private storage on this engine before execute(recording).
   * Plaintext under host OS protections; paths must never come from an untrusted renderer. */
  recordings(directory: string): Promise<ContinuationRecordingController>
}

export interface NativeContinuationControllerV1 extends NativeContinuationController {
  claimRetainedV1(
    options?: import('../../core/native-continuation-claim').NativeContinuationClaimOptions
  ): Promise<import('../reactnative/react-native-continuation-claim').ContinuationRetainedPrefixV1>
}

const SCOPE = 'desktop-native'
const missing = () => contractError('capability.unsupported', 'restoration', `${SCOPE}.continuation.native-owner`)

/** Trusted existing-central adapter, including host-private storage configuration. */
export function createNativeContinuationControlAccess(
  central: DesktopRustCoreCentral
): NativeContinuationControlAccess {
  return {
    ...(central.continuationPrepareRetainedClaimV1 &&
    central.continuationAcknowledgeRetainedClaimV1 &&
    central.continuationRetireRetainedClaimReceiptV1
      ? {
          prepareRetainedClaimV1: (items: number, bytes: number) =>
            central.continuationPrepareRetainedClaimV1!(items, bytes),
          acknowledgeRetainedClaimV1: (token: string) => central.continuationAcknowledgeRetainedClaimV1!(token),
          retireRetainedClaimReceiptV1: (token: string) => central.continuationRetireRetainedClaimReceiptV1!(token)
        }
      : {}),
    execute: (peer, declaration) => {
      if (typeof central.continuationExecute !== 'function') return Promise.reject(missing())
      return central.continuationExecute(peer, declaration)
    },
    describeBacklog: () => {
      if (typeof central.continuationDescribeBacklog !== 'function') return Promise.reject(missing())
      return central.continuationDescribeBacklog()
    },
    prepareClaim: (items, bytes) => {
      if (
        typeof central.continuationPrepareClaim !== 'function' ||
        typeof central.continuationAcknowledgeClaim !== 'function'
      )
        return Promise.reject(missing())
      return central.continuationPrepareClaim(items, bytes)
    },
    acknowledgeClaim: token => {
      if (typeof central.continuationAcknowledgeClaim !== 'function') return Promise.reject(missing())
      return central.continuationAcknowledgeClaim(token)
    }
  }
}

/** Trusted existing-central adapter with private storage configuration. */
export function createNativeContinuationController(
  central: DesktopRustCoreCentral,
  onOrderedFinished?: (
    id: string,
    result: import('../../core/continuation-recording').OrderedRecordingHostStopResult
  ) => Promise<void>
): NativeContinuationControllerV1 {
  const control = createNativeContinuationControl(createNativeContinuationControlAccess(central))
  return Object.freeze({
    ...control,
    recordings: (directory: string) =>
      rehydratePublicPromise(
        (async () => {
          if (typeof directory !== 'string' || directory.length === 0)
            throw contractError('argument.invalid', 'restoration', `${SCOPE}.continuation.recording.directory`)
          const configure = central.continuationConfigureRecordingDirectory
          if (typeof configure !== 'function' || typeof central.continuationRecordingStore !== 'function')
            throw missing()
          const configured = await call('recording.configure', () => configure.call(central, directory))
          assertContinuationRecordingConfigured(configured)
          const controller = createNativeContinuationRecordingController(
            central.continuationRecordingStore(),
            'ubm-desktop',
            async () => {
              assertDesktopOrderedReceiptVersion(central, `${SCOPE}.continuation.recording.stop-ordered`)
              return 2
            }
          )
          return Object.freeze({
            ...controller,
            stopOrdered: (id: string) =>
              rehydratePublicPromise(
                (async () => {
                  assertDesktopOrderedReceiptVersion(central, `${SCOPE}.continuation.recording.stop-ordered`)
                  const result = await controller.stopOrdered(id)
                  if (
                    onOrderedFinished &&
                    result.recording.state === 'finished' &&
                    (result.hostCleanup.state === 'released' || result.hostCleanup.state === 'not-applicable')
                  )
                    await onOrderedFinished(id, result)
                  return result
                })()
              )
          })
        })()
      )
  })
}
