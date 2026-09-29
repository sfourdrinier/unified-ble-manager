import type { BleManager } from 'unified-ble-manager'
import {
  BATTERY_SERVICE,
  BATTERY_LEVEL_CHARACTERISTIC,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
import { describeError, toJsonValue, type JsonObject } from '../../../examples-shared/driver/protocol.ts'
export { HEADLESS_CONTINUATION_TASK } from '../../../examples-shared/driver/headless-continuation-task.ts'
import { HEADLESS_CONTINUATION_TASK } from '../../../examples-shared/driver/headless-continuation-task.ts'

// Paths below are relative to example-expo/src/driver; shared code lives at
// repository root, one directory above the example application.
const WORK_BUDGET_MS = 15000
const MAX_ADMITTED_JOBS = 4

type HeadlessJob = (payload: unknown) => Promise<void>
declare global {
  var ubmReferenceHeadlessJobV1: HeadlessJob | undefined
}

/** Metro does not supply Webpack hot.data. Keep this process-owned job (and
 * its unresolved cleanup ledger) across module reevaluation; full JS restart
 * starts a new owner. Changes to task implementation require a full reload. */
export function installHeadlessContinuation(
  registry: Parameters<typeof registerHeadlessContinuation>[0],
  platform: string,
  host: Parameters<typeof createHeadlessContinuationJob>[0],
  scope: { ubmReferenceHeadlessJobV1?: HeadlessJob } = globalThis
): void {
  if (platform !== 'android' || scope.ubmReferenceHeadlessJobV1 !== undefined) return
  const job = createHeadlessContinuationJob(host)
  registerHeadlessContinuation(registry, platform, job)
  scope.ubmReferenceHeadlessJobV1 = job
}

/** A finite reference task, not a background sampling engine. Public scoped
 * connection ownership supplies disconnect and discovery cleanup. A refused
 * manager destroy remains owned here for a later invocation to retry. */
export function createHeadlessContinuationJob(host: {
  createManager(): Promise<BleManager>
  save(evidence: JsonObject): Promise<void>
  scheduleDeadline?(callback: () => void, timeoutMs: number): () => void
  now?(): number
}) {
  const now = host.now ?? Date.now
  const retained = new Set<BleManager>()
  let admittedJobs = 0
  let tail: Promise<void> = Promise.resolve()
  const schedule =
    host.scheduleDeadline ??
    ((callback, timeoutMs) => {
      const timer = setTimeout(callback, timeoutMs)
      return () => clearTimeout(timer)
    })
  async function release(manager: BleManager) {
    const receipt = await manager.destroy()
    if (receipt.state === 'released') retained.delete(manager)
    return receipt
  }
  async function run(payload: unknown): Promise<void> {
    let peerId: string | null = null
    if (typeof payload === 'object' && payload !== null && !Array.isArray(payload)) {
      const peer = Reflect.get(payload, 'peerId')
      if (
        typeof peer === 'string' &&
        /^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$/i.test(peer) &&
        Reflect.get(payload, 'event') === 'companion.appeared' &&
        Object.keys(payload).length === 2
      )
        peerId = peer
    }
    if (peerId === null) {
      await host.save({
        state: 'failed',
        operation: 'payload',
        error: { message: 'Invalid companion appearance payload' }
      })
      throw new Error('Invalid companion appearance payload')
    }
    await host.save({ state: 'started', peerId, observedAtMs: now() })
    let manager: BleManager | undefined
    let failure: { error: unknown } | undefined
    let cleanup: JsonObject = { state: 'not-acquired' }
    let batteryPercent: number | undefined
    const abort = new AbortController()
    const deadline = now() + WORK_BUDGET_MS
    const cancelDeadline = schedule(() => abort.abort(), WORK_BUDGET_MS)
    try {
      for (const owned of retained) {
        const receipt = await release(owned).catch(error => {
          cleanup = { state: 'rejected', error: toJsonValue(describeError(error)) }
          throw error
        })
        cleanup = { ...receipt, failures: toJsonValue(receipt.failures) }
        if (receipt.state !== 'released') throw new Error('Previous headless manager cleanup remains unresolved')
      }
      manager = await host.createManager()
      retained.add(manager)
      if (abort.signal.aborted || now() >= deadline) throw new Error('Headless work deadline expired before connection')
      batteryPercent = await manager.withDiscoveredConnection(
        // This H10/simulator reference job is configured for public addresses.
        // A native companion MAC is not a fresh manager's opaque peer id.
        // Other address kinds require an app-specific durable reference policy.
        { address: peerId, addressType: 'public' },
        { timeoutMs: Math.max(1, deadline - now()), signal: abort.signal },
        async ({ gatt }) => {
          if (abort.signal.aborted || now() >= deadline)
            throw new Error('Headless work deadline expired before battery read')
          return parseBatteryLevel(
            await gatt
              .characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)
              .read({ timeoutMs: Math.max(1, deadline - now()), signal: abort.signal })
          )
        }
      )
    } catch (error) {
      failure = { error }
    } finally {
      cancelDeadline()
      if (manager !== undefined) {
        try {
          const receipt = await release(manager)
          cleanup = { ...receipt, failures: toJsonValue(receipt.failures) }
          if (receipt.state !== 'released') throw new Error('Headless manager cleanup remains unresolved')
        } catch (error) {
          if (cleanup.state !== 'release-failed')
            cleanup = { state: 'rejected', error: toJsonValue(describeError(error)) }
          failure = {
            error:
              failure === undefined
                ? error
                : new AggregateError([failure.error, error], 'Headless work and cleanup failed')
          }
        }
      }
    }
    const evidence: JsonObject = {
      state: failure === undefined ? 'completed' : 'failed',
      peerId,
      observedAtMs: now(),
      cleanup,
      ...(batteryPercent === undefined ? {} : { batteryPercent }),
      ...(failure === undefined ? {} : { error: toJsonValue(describeError(failure.error)) })
    }
    try {
      await host.save(evidence)
    } catch (error) {
      throw failure === undefined
        ? error
        : new AggregateError([failure.error, error], 'Headless failure evidence could not be persisted')
    }
    if (failure !== undefined) throw failure.error
  }
  return (payload: unknown): Promise<void> => {
    // Native task bookkeeping expiry does not cancel JavaScript. Bound this
    // process-owned queue independently, including an unresolved active job.
    if (admittedJobs >= MAX_ADMITTED_JOBS) return Promise.reject(new Error('Headless task admission limit reached'))
    admittedJobs += 1
    const current = tail
      .then(() => run(payload))
      .finally(() => {
        admittedJobs -= 1
      })
    // Keep the serialization gate usable; each caller still receives its own failure.
    tail = current.catch(() => undefined)
    return current
  }
}

export function registerHeadlessContinuation(
  registry: { registerHeadlessTask(name: string, provider: () => (payload: unknown) => Promise<void>): void },
  platform: string,
  job: (payload: unknown) => Promise<void>
): void {
  if (platform === 'android') registry.registerHeadlessTask(HEADLESS_CONTINUATION_TASK, () => job)
}
