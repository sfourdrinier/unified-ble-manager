import { BleError } from './public/errors'
import { toPublicCleanupRecord, type CleanupRecord } from './public/cleanup'
import type { CleanupRecord as InternalCleanupRecord } from './backend-contract/errors'
import { rehydratePublicPromise } from './public/error-bridge'

/** Failed initialization whose exact native cleanup remains retryable. Opens no new radio. */
export class DesktopProcessHostInitializationError extends BleError {
  constructor(
    readonly originalCause: unknown,
    readonly cleanupCause: unknown,
    readonly retryCleanup: () => Promise<CleanupRecord>
  ) {
    super('platform.failure', 'cleanup', 'desktop-process-host.initialization-cleanup', {
      retryability: 'caller-decides'
    })
  }
}

/** Compensate the allocated owner; a rejected admission never loses its cleanup handle. */
export async function compensateDesktopInitialization(
  originalCause: unknown,
  destroy: () => Promise<InternalCleanupRecord>
): Promise<never> {
  await cleanupDesktopAllocation(originalCause, destroy)
  throw originalCause
}

/** Release a temporary allocation without hiding a failed receipt in adapter metadata. */
export async function cleanupDesktopAllocation(
  originalCause: unknown,
  destroy: () => Promise<InternalCleanupRecord>
): Promise<void> {
  let cleanup: Promise<CleanupRecord> | null = null
  const retryCleanup = () => {
    if (cleanup === null)
      cleanup = rehydratePublicPromise(Promise.resolve().then(destroy).then(toPublicCleanupRecord)).then(
        result => {
          if (result.state !== 'released') cleanup = null
          return result
        },
        error => {
          cleanup = null
          throw error
        }
      )
    return cleanup
  }
  let receipt
  try {
    receipt = await retryCleanup()
  } catch (cleanupCause) {
    throw new DesktopProcessHostInitializationError(originalCause, cleanupCause, retryCleanup)
  }
  if (receipt.state !== 'released')
    throw new DesktopProcessHostInitializationError(originalCause, receipt, retryCleanup)
}
