import { contractError, type PlatformErrorDetail } from '../../backend-contract/errors'

/**
 * Safety bound on a native CoreBluetooth release call during teardown.
 *
 * Deliberately independent of any caller deadline: cleanup runs after the owning
 * operation has already ended, so inheriting its (often expired) deadline would
 * report every release as failed. Matches the BlueZ and WinRT cleanup bounds so
 * the same logical teardown is governed identically on every desktop backend.
 */
export const CORE_BLUETOOTH_CLEANUP_TIMEOUT_MS = 1_000

export function withCoreBluetoothCleanupTimeout<Result>(
  operation: () => Promise<Result>,
  operationName: string,
  platform: PlatformErrorDetail | null = null
): Promise<Result> {
  let timer: ReturnType<typeof setTimeout> | null = null
  const timeout = new Promise<Result>((_resolve, reject) => {
    timer = setTimeout(
      () => reject(contractError('operation.timed-out', 'cleanup', operationName, platform)),
      CORE_BLUETOOTH_CLEANUP_TIMEOUT_MS
    )
  })
  const source = Promise.resolve().then(operation)
  return Promise.race([source, timeout]).finally(() => {
    if (timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  })
}
