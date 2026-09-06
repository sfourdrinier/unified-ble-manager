// src/backends/winrt/winrt-subscription-runtime.ts

import type { CleanupFailure, CleanupRecord } from '../../backend-contract/errors'
import type { CharacteristicPath } from '../../backend-contract/gatt'
import type { OperationTerminalRecord } from '../../backend-contract/operations'
import { byteLimit, ownBytes, type GenerationId, type OwnedBytes } from '../../backend-contract/primitives'
import {
  WinRtBackendSubscription,
  WinRtSubscriptionStream,
  characteristicAddressKey,
  cleanupFailure,
  releasedCleanup
} from './winrt-handles'
import type { WinRtBackend, WinRtConnectionRecord, WinRtPhysicalSubscription } from './winrt-backend'
import type { WinRtCancellationState, WinRtCharacteristicAddress } from './winrt-boundary'
import {
  timedOutWinRtCleanup,
  waitForWinRtValue,
  WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
  winRtPlatformError
} from './winrt-backend-helpers'

const maximumValueBytes = byteLimit(512 * 1024)
const WINRT_ENABLEMENT_STAGING_ITEM_LIMIT = 16
const WINRT_ENABLEMENT_STAGING_BYTE_LIMIT = 64 * 1024
const latestPhysicalCleanupFailure = new WeakMap<WinRtPhysicalSubscription, CleanupRecord>()

export function physicalSubscriptionKey(
  address: WinRtCharacteristicAddress,
  connectionGeneration: GenerationId<'connection-generation', string>
): string {
  return `${characteristicAddressKey(address)}\u0000${String(connectionGeneration)}`
}

/** Owns the physical CCCD reference count and retryable native disable cleanup. */
export function stopWinRtPhysicalSubscription(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription
): Promise<CleanupRecord> {
  if (physical.removal !== null) {
    return physical.removal
  }
  if (physical.removalSettlement !== null) {
    return trackBoundedPhysicalRemoval(backend, physical, physical.removalSettlement)
  }
  const removalPhase = physical.enableConfirmed ? 'post-enable' : 'pre-enable'
  physical.state = 'removing'
  let nativeCompletion: Promise<void>
  try {
    nativeCompletion = backend.boundary.stopNotify(physical.address).completion
  } catch (error) {
    physical.state = 'cleanup-pending'
    physical.removalPhase = null
    const failure = winRtSubscriptionCleanupFailure(error)
    latestPhysicalCleanupFailure.set(physical, failure)
    return Promise.resolve(failure)
  }
  const settlement: Promise<CleanupRecord> = nativeCompletion.then(
    () => {
      latestPhysicalCleanupFailure.delete(physical)
      physical.removalSettlement = null
      if (removalPhase === 'post-enable') {
        physical.removalPhase = null
      }
      if (
        (removalPhase === 'post-enable' || physical.enableOutcome === 'failed') &&
        backend.subscriptions.get(physical.key) === physical
      ) {
        backend.subscriptions.delete(physical.key)
      }
      return releasedCleanup
    },
    error => {
      if (physical.removalSettlement === settlement) {
        physical.removalSettlement = null
        physical.state = 'cleanup-pending'
        physical.removal = null
        physical.removalPhase = null
      }
      const failure = winRtSubscriptionCleanupFailure(error)
      latestPhysicalCleanupFailure.set(physical, failure)
      return failure
    }
  )
  physical.removalSettlement = settlement
  physical.removalPhase = removalPhase
  return trackBoundedPhysicalRemoval(backend, physical, settlement)
}

function trackBoundedPhysicalRemoval(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  settlement: Promise<CleanupRecord>
): Promise<CleanupRecord> {
  const removal = waitForWinRtValue(settlement, backend.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS, backend.now).then(
    result => {
      if (result.state === 'timed-out') {
        physical.state = 'cleanup-pending'
        const failure = timedOutWinRtCleanup('subscription', 'winrt.gatt.stop-notify')
        latestPhysicalCleanupFailure.set(physical, failure)
        return failure
      }
      return result.value
    }
  )
  physical.removal = removal
  removal.then(result => {
    if (result.state === 'release-failed' && physical.removal === removal) {
      physical.removal = null
    }
  })
  return removal
}

/** Ensures an enable that completed after invalidation receives a distinct, ordered disable. */
export async function stopWinRtPhysicalSubscriptionAfterEnable(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription
): Promise<CleanupRecord> {
  physical.enableConfirmed = true
  const preEnableRemoval = physical.removalPhase === 'pre-enable' ? physical.removal : null
  if (preEnableRemoval !== null) {
    const cleanup = await preEnableRemoval
    if (cleanup.state === 'release-failed' && physical.removalSettlement !== null) {
      return cleanup
    }
    if (physical.removal === preEnableRemoval) {
      physical.removal = null
      physical.removalPhase = null
    }
  }
  if (!winRtPhysicalOwnsCurrentGeneration(backend, physical)) {
    // Native StopNotify is keyed only by peer+characteristic. A replacement generation
    // may already own that CCCD; disabling it would drop the live subscription.
    releaseWinRtPhysicalSubscriptionRecord(backend, physical)
    return releasedCleanup
  }
  return stopWinRtPhysicalSubscription(backend, physical)
}

function winRtPhysicalOwnsCurrentGeneration(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription
): boolean {
  const record = backend.connectionsByNativeId.get(physical.address.nativePeerId)
  return record !== undefined && record.connectionGeneration === physical.connectionGeneration
}

function releaseWinRtPhysicalSubscriptionRecord(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription
): void {
  if (backend.subscriptions.get(physical.key) === physical) {
    backend.subscriptions.delete(physical.key)
  }
  physical.removalPhase = null
}

/** Invalidates every pending waiter before starting retryable physical teardown. */
export async function invalidateWinRtPhysicalSubscription(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  terminalError: Error
): Promise<CleanupRecord> {
  physical.invalidated = true
  discardWinRtStagedNotifications(physical)
  for (const waiter of physical.pendingConsumers) {
    if (waiter.state !== 'pending') {
      continue
    }
    waiter.state = 'invalidated'
    waiter.physical = null
    waiter.invalidationError = terminalError
    waiter.invalidate(terminalError)
  }
  physical.pendingConsumers.clear()
  const cancellation = requestWinRtPhysicalEnableCancellation(physical)
  const cleanup = stopWinRtPhysicalSubscription(backend, physical)
  const failures: CleanupFailure[] = []
  try {
    const cancelled = await waitForWinRtValue(
      cancellation,
      backend.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
      backend.now
    )
    if (cancelled.state === 'timed-out') {
      failures.push(...timedOutWinRtCleanup('operation', 'winrt.gatt.start-notify.cancel').failures)
    }
  } catch (error) {
    failures.push(...cleanupFailure('operation', 'winrt.gatt.start-notify.cancel', error).failures)
  }
  const initialCleanup = await cleanup
  failures.push(...initialCleanup.failures)
  const enablement = physical.enablement
  if (enablement !== null) {
    const settled = await waitForWinRtValue(
      enablement.then(
        () => undefined,
        () => undefined
      ),
      backend.now() + WINRT_NATIVE_CLEANUP_TIMEOUT_MS,
      backend.now
    )
    if (settled.state === 'timed-out') {
      failures.push(...timedOutWinRtCleanup('subscription', 'winrt.gatt.start-notify.enablement').failures)
    } else {
      const terminalCleanup = latestPhysicalCleanupFailure.get(physical)
      if (terminalCleanup !== undefined && terminalCleanup !== initialCleanup) {
        failures.push(...terminalCleanup.failures)
      }
      if (
        backend.subscriptions.get(physical.key) === physical &&
        latestPhysicalCleanupFailure.get(physical) === undefined
      ) {
        failures.push(
          ...cleanupFailure(
            'subscription',
            'winrt.gatt.stop-notify',
            new Error('WinRT physical subscription remained owned after terminal enablement settlement')
          ).failures
        )
      }
    }
  }
  return failures.length === 0
    ? releasedCleanup
    : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
}

export function requestWinRtPhysicalEnableCancellation(
  physical: WinRtPhysicalSubscription
): Promise<WinRtCancellationState> {
  if (physical.enableCancellation !== null) {
    return physical.enableCancellation
  }
  physical.enableCancellationRequested = true
  if (physical.nativeEnable === null) {
    return Promise.resolve(physical.enableOutcome === 'pending' ? 'cancellation-requested' : 'already-terminal')
  }
  let cancellation: Promise<WinRtCancellationState>
  try {
    cancellation = physical.nativeEnable.cancel()
  } catch (error) {
    cancellation = Promise.reject(error)
  }
  const trackedCancellation = cancellation.then(
    state => state,
    error => {
      if (physical.enableCancellation === trackedCancellation) {
        physical.enableCancellation = null
      }
      throw error
    }
  )
  physical.enableCancellation = trackedCancellation
  return trackedCancellation
}

export function removeWinRtSubscription(
  backend: WinRtBackend,
  subscription: WinRtBackendSubscription
): Promise<CleanupRecord> {
  const physical = subscription.physical
  if (subscription.removed) {
    return physical.consumers.size === 0 &&
      physical.pendingConsumers.size === 0 &&
      backend.subscriptions.get(physical.key) === physical
      ? stopWinRtPhysicalSubscription(backend, physical)
      : Promise.resolve(releasedCleanup)
  }
  subscription.removed = true
  subscription.stream.closeWithReason('owner-released')
  physical.consumers.delete(subscription)
  return physical.consumers.size === 0 && physical.pendingConsumers.size === 0
    ? stopWinRtPhysicalSubscription(backend, physical)
    : Promise.resolve(releasedCleanup)
}

export function createWinRtSubscription(
  backend: WinRtBackend,
  connectionRecord: WinRtConnectionRecord,
  physical: WinRtPhysicalSubscription,
  path: CharacteristicPath<string, string, string, string, string, 'current'>,
  terminal: OperationTerminalRecord<string, string>,
  stream: WinRtSubscriptionStream
): WinRtBackendSubscription {
  const subscription = new WinRtBackendSubscription(
    backend,
    connectionRecord,
    physical,
    path,
    backend.identifiers().subscriptionId(`winrt-subscription-${backend.nextSubscription}`),
    terminal,
    stream
  )
  backend.nextSubscription += 1
  physical.consumers.add(subscription)
  stream.bindOwnerRemoval(() => removeWinRtSubscription(backend, subscription))
  flushWinRtStagedNotifications(backend, physical, subscription)
  return subscription
}

export function createWinRtPhysicalSubscription(
  backend: WinRtBackend,
  address: WinRtCharacteristicAddress,
  mode: 'notify' | 'indicate',
  connectionGeneration: GenerationId<'connection-generation', string>
): WinRtPhysicalSubscription {
  const physical: WinRtPhysicalSubscription = {
    key: physicalSubscriptionKey(address, connectionGeneration),
    address,
    connectionGeneration,
    mode,
    consumers: new Set(),
    pendingConsumers: new Set(),
    stagedValues: [],
    state: 'enabling',
    enableConfirmed: false,
    enableOutcome: 'pending',
    invalidated: false,
    enablement: null,
    nativeEnable: null,
    enableCancellationRequested: false,
    enableCancellation: null,
    removal: null,
    removalSettlement: null,
    removalPhase: null,
    stagedBytes: 0,
    stagingOverflowed: false
  }
  backend.subscriptions.set(physical.key, physical)
  return physical
}

/** Delivers native ingress, releasing only consumers terminalized by error-policy overflow. */
export function emitWinRtNotification(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  source: unknown
): void {
  if (physical.invalidated || physical.state === 'removing' || physical.state === 'cleanup-pending') {
    return
  }
  if (physical.state === 'enabling' || (physical.state === 'ready' && physical.consumers.size === 0)) {
    stageWinRtNotification(backend, physical, source)
    return
  }
  if (physical.state !== 'ready') {
    return
  }
  try {
    deliverWinRtNotification(backend, physical, copyWinRtNotificationBytes(source))
  } catch (error) {
    terminalizeWinRtNotificationIngressFailure(
      backend,
      physical,
      winRtPlatformError('gatt.subscribe-failed', 'gatt', 'winrt.gatt.notify.ingress', error)
    )
  }
}

function copyWinRtNotificationBytes(source: unknown): OwnedBytes {
  // The N-API callback runs in this V8 realm, so this exact check excludes wider views and array-like values.
  if (!(source instanceof Uint8Array)) {
    throw new TypeError('WinRT notification ingress payload must be a Uint8Array')
  }
  return ownBytes(source, maximumValueBytes)
}

function stageWinRtNotification(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  source: unknown
): void {
  try {
    const copied = copyWinRtNotificationBytes(source)
    if (physical.stagingOverflowed) {
      return
    }
    if (
      physical.stagedValues.length >= WINRT_ENABLEMENT_STAGING_ITEM_LIMIT ||
      physical.stagedBytes + copied.byteLength > WINRT_ENABLEMENT_STAGING_BYTE_LIMIT
    ) {
      physical.stagingOverflowed = true
      return
    }
    physical.stagedValues.push(copied)
    physical.stagedBytes += copied.byteLength
  } catch (error) {
    terminalizeWinRtNotificationIngressFailure(
      backend,
      physical,
      winRtPlatformError('gatt.subscribe-failed', 'gatt', 'winrt.gatt.notify.ingress', error)
    )
  }
}

function flushWinRtStagedNotifications(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  subscription: WinRtBackendSubscription
): void {
  try {
    for (const copied of physical.stagedValues) {
      const emission = emitWinRtCopiedNotification(subscription, physical, copied)
      if (emission.terminated && subscription.stream.overflowPolicy === 'error') {
        releaseWinRtOverflowedSubscription(backend, subscription)
        discardWinRtStagedNotifications(physical)
        return
      }
    }
    if (physical.stagingOverflowed) {
      subscription.stream.finishWithReason('overflow')
    }
  } finally {
    if (physical.pendingConsumers.size === 0) {
      discardWinRtStagedNotifications(physical)
    }
  }
}

function deliverWinRtNotification(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  copied: OwnedBytes
): void {
  for (const consumer of physical.consumers) {
    const emission = emitWinRtCopiedNotification(consumer, physical, copied)
    if (emission.terminated && consumer.stream.overflowPolicy === 'error') {
      releaseWinRtOverflowedSubscription(backend, consumer)
    }
  }
}

function emitWinRtCopiedNotification(
  consumer: WinRtBackendSubscription,
  physical: WinRtPhysicalSubscription,
  copied: OwnedBytes
) {
  return consumer.stream.emit(
    Object.freeze({ value: ownBytes(copied, maximumValueBytes), indication: physical.mode === 'indicate' }),
    copied.byteLength
  )
}

export function discardWinRtStagedNotifications(physical: WinRtPhysicalSubscription): void {
  physical.stagedValues.length = 0
  physical.stagedBytes = 0
  physical.stagingOverflowed = false
}

/** Removes just an overflow-terminal consumer; the last consumer owns physical CCCD teardown. */
function releaseWinRtOverflowedSubscription(backend: WinRtBackend, subscription: WinRtBackendSubscription): void {
  let cleanup: Promise<CleanupRecord>
  try {
    cleanup = removeWinRtSubscription(backend, subscription)
  } catch (error) {
    reportWinRtNotificationIngressFailure('[WinRtSubscription.overflow] Subscription cleanup rejected:', error)
    return
  }
  cleanup.then(
    result => {
      if (result.state === 'release-failed') {
        reportWinRtNotificationIngressFailure(
          '[WinRtSubscription.overflow] Physical CCCD cleanup requires retry:',
          result.failures
        )
      }
    },
    error => reportWinRtNotificationIngressFailure('[WinRtSubscription.overflow] Subscription cleanup rejected:', error)
  )
}

function terminalizeWinRtNotificationIngressFailure(
  backend: WinRtBackend,
  physical: WinRtPhysicalSubscription,
  terminalError: Error
): void {
  try {
    for (const consumer of physical.consumers) {
      consumer.removed = true
      consumer.stream.closeWithReason('source-failed')
    }
    physical.consumers.clear()
    reportWinRtNotificationIngressFailure(
      '[WinRtSubscription.ingress] Native notification ingress failed:',
      terminalError
    )
    const cleanup = invalidateWinRtPhysicalSubscription(backend, physical, terminalError)
    cleanup.then(
      result => {
        if (result.state === 'release-failed') {
          reportWinRtNotificationIngressFailure(
            '[WinRtSubscription.ingress] Physical CCCD cleanup requires retry:',
            result.failures
          )
        }
      },
      error =>
        reportWinRtNotificationIngressFailure('[WinRtSubscription.ingress] Physical CCCD cleanup rejected:', error)
    )
  } catch (error) {
    reportWinRtNotificationIngressFailure('[WinRtSubscription.ingress] Terminalization failed:', error)
  }
}

/** Native callbacks cannot propagate an observer failure back through the WinRT boundary. */
function reportWinRtNotificationIngressFailure(message: string, detail: unknown): void {
  try {
    console.error(message, detail)
  } catch {
    // The ingress path has already terminalized ownership; a diagnostics observer cannot reopen it.
  }
}

function winRtSubscriptionCleanupFailure(error: unknown): CleanupRecord {
  return cleanupFailure(
    'subscription',
    'winrt.gatt.stop-notify',
    winRtPlatformError('gatt.subscribe-failed', 'gatt', 'winrt.gatt.stop-notify', error)
  )
}
