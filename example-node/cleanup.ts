import { StopAllError, type ScenarioRegistry } from '../examples-shared/driver/scenario-core.ts'
import { describeError } from '../examples-shared/driver/protocol.ts'
import type { CleanupRecord } from 'unified-ble-manager'

export function isConfirmedNodeRelease(receipt: unknown): boolean {
  return (
    typeof receipt === 'object' &&
    receipt !== null &&
    'state' in receipt &&
    receipt.state === 'released' &&
    'failures' in receipt &&
    Array.isArray(receipt.failures) &&
    receipt.failures.length === 0
  )
}

/** A failed server shutdown must not fall out of the event loop as exit 0.
 * Keep the same process alive, serialize signals, and retry only on request. */
export function createNodeShutdownHandler(actions: {
  cleanup(): Promise<void>
  released(): Promise<void>
  failed(error: unknown): Promise<void>
}): () => Promise<void> {
  let pending: Promise<void> | null = null
  let retained: ReturnType<typeof setInterval> | undefined
  return () => {
    if (pending !== null) return pending
    retained ??= setInterval(() => {}, 60_000)
    pending = (async () => {
      try {
        await actions.cleanup()
        await actions.released()
        clearInterval(retained)
        retained = undefined
      } catch (error) {
        await actions.failed(error)
      }
    })().finally(() => {
      pending = null
    })
    return pending
  }
}

/** Scenarios hand off native data first; the process owner closes last. A
 * failed stage cannot suppress another owner's teardown or its diagnostics. */
export async function shutdownNodeDriver(
  registry: Pick<ScenarioRegistry, 'stopAll'>,
  host: { destroy(): Promise<CleanupRecord> },
  report: (value: unknown) => void
): Promise<void> {
  const errors: unknown[] = []
  try {
    await stopNodeDriverScenarios(registry, report)
  } catch (error) {
    errors.push(error)
    report({ type: 'shutdown-scenarios-failed', error: describeError(error) })
  }
  try {
    const receipt = await host.destroy()
    report({ type: 'shutdown-process-host', receipt })
    if (!isConfirmedNodeRelease(receipt)) {
      throw new Error('Process host cleanup remains unconfirmed; retain this owner for retry')
    }
  } catch (error) {
    errors.push(error)
    report({ type: 'shutdown-process-host-failed', error: describeError(error) })
  }
  if (errors.length > 0) throw new AggregateError(errors, 'Node driver shutdown retained cleanup failures')
}

/** Attempt every owner, retain every failure, and never report a clean CLI exit
 * merely because failures were printed. A remote host can retry before exit. */
export async function stopNodeDriverScenarios(
  registry: Pick<ScenarioRegistry, 'stopAll'>,
  report: (value: unknown) => void
): Promise<void> {
  try {
    const result = await registry.stopAll()
    report({ type: 'shutdown-stop-all', report: result })
  } catch (error) {
    if (error instanceof StopAllError) report({ type: 'shutdown-stop-all', report: error.report })
    throw error
  }
}
