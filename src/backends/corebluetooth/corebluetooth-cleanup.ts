import { contractError } from '../../backend-contract/errors'

export const CORE_BLUETOOTH_CLEANUP_TIMEOUT_MS = 1_000

export function withCoreBluetoothCleanupTimeout<Result>(
  operation: () => Promise<Result>,
  operationName: string
): Promise<Result> {
  let timer: ReturnType<typeof setTimeout> | null = null
  const timeout = new Promise<Result>((_resolve, reject) => {
    timer = setTimeout(
      () => reject(contractError('operation.timed-out', 'cleanup', operationName)),
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
