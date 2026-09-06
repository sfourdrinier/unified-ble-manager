// src/backends/bluez/bluez-scan-runtime.ts

import type { OwnerScanOptions } from '../../backend-contract/advertisement'
import { BackendContractError, contractError, type CleanupRecord } from '../../backend-contract/errors'
import { deadline, type ClientId, type LeaseId, type ScanShareToken } from '../../backend-contract/primitives'
import { CoreBoundedStream } from '../../core/bounded-stream'
import type { BluezBackendRuntime } from './bluez-backend-runtime'
import type { BluezScanConsumer, BluezScanGroup } from './bluez-runtime-types'
import { BLUEZ_ADAPTER_INTERFACE, BLUEZ_DEVICE_INTERFACE } from './bluez-dbus-contract'
import { BluezScanLease, releasedBluezCleanup } from './bluez-backend-handles'
import { scanFilterVariant, scanSignature } from './bluez-runtime-models'
import {
  awaitBluezNativePromise,
  awaitSharedBluezTransition,
  BLUEZ_NATIVE_CLEANUP_TIMEOUT_MS,
  isBluezCallerTerminal,
  waitForBluezBoolean
} from './bluez-property-waiters'

export async function startBluezScan(
  runtime: BluezBackendRuntime,
  options: OwnerScanOptions<string, string>,
  clientId: ClientId<string, string>
): Promise<BluezScanLease> {
  runtime.assertUsable('bluez.scan.start')
  assertScanAdmission(runtime, options)
  if (runtime.scanGroup?.stopRequested === true) {
    const orphanedOwner = runtime.scanGroup.consumers.get(String(runtime.scanGroup.ownerLeaseId))
    if (orphanedOwner === undefined) {
      throw contractError('lifecycle.invariant-violation', 'scan', 'bluez.scan.retry-orphaned-stop')
    }
    await runtime.stopScan(orphanedOwner)
  }
  if (runtime.scanGroup !== null) {
    throw contractError('scan.already-active', 'scan', 'bluez.scan.start')
  }
  const ids = runtime.identifiers()
  const leaseId = ids.leaseId(`bluez-scan-${runtime.nextScan}`)
  const scanSessionId = ids.scanSessionId(`bluez-scan-session-${runtime.nextScan}`)
  const shareToken = options.sharing.allowSharing ? ids.scanShareToken(`bluez-scan-share-${runtime.nextScan}`) : null
  runtime.nextScan += 1
  const abort = (): void => {
    if (runtime.scanGroup?.state === 'starting') {
      runtime.scanGroup.stopRequested = true
      return
    }
    observeScanCleanup(runtime.stopScan(consumer))
  }
  const consumer: BluezScanConsumer = {
    scanSessionId,
    leaseId,
    shareToken,
    clientId,
    options,
    stream: new CoreBoundedStream(options.delivery, options.delivery.overflowPolicy),
    abort,
    deadlineTimer: null,
    stopped: null
  }
  let settleStartup: (() => void) | null = null
  const startupSettled = new Promise<void>(resolve => {
    settleStartup = resolve
  })
  if (settleStartup === null) {
    throw contractError('lifecycle.invariant-violation', 'scan', 'bluez.scan.startup-signal')
  }
  const group: BluezScanGroup = {
    ownerLeaseId: leaseId,
    shareToken,
    signature: scanSignature(options),
    consumers: new Map([[String(leaseId), consumer]]),
    state: 'starting',
    physicalStarted: false,
    filterApplied: false,
    filterSettled: false,
    discoverySettled: false,
    setFilter: null,
    startDiscovery: null,
    stopDiscoveryRequested: false,
    stopDiscovery: null,
    filterClearRequested: false,
    filterClear: null,
    stopRequested: false,
    resetRequested: false,
    startupComplete: false,
    startupSettled,
    settleStartup
  }
  runtime.scanGroup = group
  options.signal?.addEventListener('abort', abort, { once: true })
  try {
    const setFilter = runtime.boundary.methods.callVoid(
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'SetDiscoveryFilter',
      [scanFilterVariant(options)]
    )
    group.setFilter = setFilter
    trackBluezScanAcquisition(
      runtime,
      group,
      consumer,
      setFilter,
      () => {
        group.filterApplied = true
        group.filterSettled = true
      },
      () => {
        group.filterSettled = true
      }
    )
    await awaitSharedBluezTransition(setFilter, options, runtime.now, 'bluez.scan.set-filter')
  } catch (primaryError) {
    await failBluezScanStartup(runtime, group, consumer, primaryError)
  }
  const filterStartupFailure = scanStartupFailure(runtime, group)
  if (filterStartupFailure !== null) {
    await failBluezScanStartup(runtime, group, consumer, filterStartupFailure)
  }
  const filterAdmissionFailure = scanAdmissionFailure(runtime, options)
  if (group.stopRequested || filterAdmissionFailure !== null) {
    await failBluezScanStartup(
      runtime,
      group,
      consumer,
      filterAdmissionFailure ?? contractError('operation.aborted', 'scan', 'bluez.scan.start')
    )
  }
  try {
    const startDiscovery = runtime.boundary.methods.callVoid(
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'StartDiscovery',
      []
    )
    group.startDiscovery = startDiscovery
    trackBluezScanAcquisition(
      runtime,
      group,
      consumer,
      startDiscovery,
      () => {
        group.physicalStarted = true
        group.discoverySettled = true
      },
      () => {
        group.discoverySettled = true
      }
    )
    await awaitSharedBluezTransition(startDiscovery, options, runtime.now, 'bluez.scan.start-discovery')
    group.physicalStarted = true
    const discoveryStartupFailure = scanStartupFailure(runtime, group)
    if (discoveryStartupFailure !== null) {
      await failBluezScanStartup(runtime, group, consumer, discoveryStartupFailure)
    }
    await waitForBluezBoolean(
      runtime,
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'Discovering',
      true,
      options
    )
  } catch (primaryError) {
    await failBluezScanStartup(runtime, group, consumer, primaryError)
  }
  const discoveryAdmissionFailure = scanAdmissionFailure(runtime, options)
  if (group.stopRequested || discoveryAdmissionFailure !== null) {
    await failBluezScanStartup(
      runtime,
      group,
      consumer,
      discoveryAdmissionFailure ?? contractError('operation.aborted', 'scan', 'bluez.scan.start')
    )
  }
  group.state = 'active'
  if (options.deadline !== null) {
    consumer.deadlineTimer = setTimeout(abort, Math.max(0, options.deadline - runtime.now()))
  }
  for (const path of runtime.store.objectsWithInterface(BLUEZ_DEVICE_INTERFACE)) {
    runtime.emitAdvertisementForPath(path)
  }
  group.startupComplete = true
  group.settleStartup()
  return new BluezScanLease(runtime, consumer)
}

export async function joinBluezScan(
  runtime: BluezBackendRuntime,
  leaseId: LeaseId<string, string>,
  shareToken: ScanShareToken<string, string>,
  clientId: ClientId<string, string>
): Promise<BluezScanLease> {
  runtime.assertUsable('bluez.scan.join')
  const group = runtime.scanGroup
  if (
    group === null ||
    group.state !== 'active' ||
    group.ownerLeaseId !== leaseId ||
    group.shareToken === null ||
    group.shareToken !== shareToken
  ) {
    throw contractError('ownership.denied', 'scan', 'bluez.scan.join')
  }
  const owner = group.consumers.get(String(group.ownerLeaseId))
  if (owner === undefined) {
    throw contractError('lifecycle.invariant-violation', 'scan', 'bluez.scan.join')
  }
  const ids = runtime.identifiers()
  const joinedLeaseId = ids.leaseId(`bluez-scan-${runtime.nextScan}`)
  const consumer: BluezScanConsumer = {
    scanSessionId: owner.scanSessionId,
    leaseId: joinedLeaseId,
    shareToken: null,
    clientId,
    options: owner.options,
    stream: new CoreBoundedStream(owner.options.delivery, owner.options.delivery.overflowPolicy),
    abort: null,
    deadlineTimer: null,
    stopped: null
  }
  runtime.nextScan += 1
  group.consumers.set(String(joinedLeaseId), consumer)
  return new BluezScanLease(runtime, consumer)
}

export async function stopBluezScan(runtime: BluezBackendRuntime, consumer: BluezScanConsumer): Promise<CleanupRecord> {
  if (consumer.abort !== null) {
    consumer.options.signal?.removeEventListener('abort', consumer.abort)
  }
  if (consumer.deadlineTimer !== null) {
    clearTimeout(consumer.deadlineTimer)
    consumer.deadlineTimer = null
  }
  const group = runtime.scanGroup
  if (group === null || !group.consumers.has(String(consumer.leaseId))) {
    consumer.stream.closeWithReason('owner-released')
    return releasedBluezCleanup
  }
  if (consumer.leaseId !== group.ownerLeaseId) {
    group.consumers.delete(String(consumer.leaseId))
    consumer.stream.closeWithReason('owner-released')
    return releasedBluezCleanup
  }
  if (!group.physicalStarted && group.state === 'starting') {
    group.stopRequested = true
  }
  group.state = 'stopping'
  try {
    await adoptPendingBluezScanAcquisition(runtime, group)
  } catch (error) {
    if (isBluezCleanupTimeout(error)) {
      return pendingBluezScanCleanup('bluez.scan.startup-method')
    }
    throw error
  }
  if (group.physicalStarted) {
    try {
      await stopBluezPhysicalDiscovery(runtime, group)
    } catch (error) {
      if (isBluezCleanupTimeout(error)) {
        return pendingBluezScanCleanup('bluez.scan.stop-discovery')
      }
      if (runtime.scanGroup === group) {
        group.state = 'active'
      }
      console.error('[stopBluezScan] BlueZ StopDiscovery failed; scan ownership retained for retry:', error)
      throw error
    }
  }
  try {
    await clearBluezDiscoveryFilter(runtime, group)
  } catch (error) {
    if (isBluezCleanupTimeout(error)) {
      return pendingBluezScanCleanup('bluez.scan.discovery-filter')
    }
    group.state = 'active'
    console.error('[stopBluezScan] BlueZ discovery-filter cleanup failed; scan ownership retained for retry:', error)
    throw error
  }
  for (const joined of [...group.consumers.values()]) {
    joined.stream.closeWithReason('owner-released')
  }
  group.consumers.clear()
  runtime.scanGroup = null
  return releasedBluezCleanup
}

export async function destroyBluezScan(runtime: BluezBackendRuntime): Promise<CleanupRecord> {
  const initialGroup = runtime.scanGroup
  if (initialGroup === null) {
    return releasedBluezCleanup
  }
  if (!initialGroup.startupComplete) {
    initialGroup.stopRequested = true
    try {
      await awaitSharedBluezTransition(
        initialGroup.startupSettled,
        { signal: null, deadline: deadline(runtime.now() + BLUEZ_NATIVE_CLEANUP_TIMEOUT_MS) },
        runtime.now,
        'bluez.destroy.scan-start'
      )
    } catch (error) {
      if (isBluezCleanupTimeout(error)) {
        return pendingBluezScanCleanup('bluez.destroy.scan-start')
      }
      throw error
    }
  }
  const group = runtime.scanGroup
  if (group === null) {
    return releasedBluezCleanup
  }
  const owner = group.consumers.get(String(group.ownerLeaseId))
  if (owner === undefined) {
    throw contractError('lifecycle.invariant-violation', 'scan', 'bluez.destroy.scan-owner')
  }
  return runtime.stopScan(owner)
}

async function failBluezScanStartup(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  consumer: BluezScanConsumer,
  primaryError: unknown
): Promise<never> {
  if (isBluezCallerTerminal(primaryError) && bluezScanAcquisitionPending(group)) {
    group.stopRequested = true
    settleBluezScanStartup(group)
    observeScanCleanup(runtime.stopScan(consumer))
    throw primaryError
  }
  const cleanupErrors: unknown[] = []
  const ownsGroup = runtime.scanGroup === group
  if (group.physicalStarted && ownsGroup) {
    try {
      await stopBluezPhysicalDiscovery(runtime, group)
    } catch (cleanupError) {
      cleanupErrors.push(cleanupError)
      console.error('[startBluezScan] Failed to stop BlueZ discovery after start failure:', cleanupError)
    }
  }
  if (ownsGroup) {
    try {
      await clearBluezDiscoveryFilter(runtime, group)
    } catch (cleanupError) {
      cleanupErrors.push(cleanupError)
      console.error('[startBluezScan] Failed to clear the BlueZ discovery filter after start failure:', cleanupError)
    }
  }
  if (cleanupErrors.length > 0) {
    retainFailedScanStartup(runtime, group, consumer)
    throw new AggregateError([primaryError, ...cleanupErrors], 'BlueZ scan start and cleanup both failed')
  }
  releaseFailedScanStartup(runtime, group, consumer)
  throw primaryError
}

function releaseFailedScanStartup(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  consumer: BluezScanConsumer
): void {
  if (consumer.abort !== null) {
    consumer.options.signal?.removeEventListener('abort', consumer.abort)
  }
  if (consumer.deadlineTimer !== null) {
    clearTimeout(consumer.deadlineTimer)
    consumer.deadlineTimer = null
  }
  if (runtime.scanGroup === group) {
    runtime.scanGroup = null
  }
  consumer.stream.closeWithReason('owner-released')
  group.startupComplete = true
  group.settleStartup()
}

function retainFailedScanStartup(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  consumer: BluezScanConsumer
): void {
  if (consumer.abort !== null) {
    consumer.options.signal?.removeEventListener('abort', consumer.abort)
  }
  if (consumer.deadlineTimer !== null) {
    clearTimeout(consumer.deadlineTimer)
    consumer.deadlineTimer = null
  }
  if (runtime.scanGroup === group) {
    group.state = 'active'
    group.stopRequested = true
  }
  consumer.stream.closeWithReason('owner-released')
  group.startupComplete = true
  group.settleStartup()
}

async function clearBluezDiscoveryFilter(runtime: BluezBackendRuntime, group: BluezScanGroup): Promise<void> {
  if (!group.filterClearRequested) {
    group.filterClearRequested = true
    const filterClear = runtime.boundary.methods.callVoid(
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'SetDiscoveryFilter',
      [{ signature: 'a{sv}', value: Object.freeze({}) }]
    )
    group.filterClear = filterClear
    filterClear.catch(() => {
      if (group.filterClear === filterClear) {
        group.filterClear = null
        group.filterClearRequested = false
      }
    })
  }
  const filterClear = group.filterClear
  if (filterClear === null) {
    throw contractError('lifecycle.invariant-violation', 'scan', 'bluez.scan.discovery-filter')
  }
  await awaitBluezNativePromise(
    filterClear,
    runtime.now,
    'bluez.scan.discovery-filter',
    BLUEZ_NATIVE_CLEANUP_TIMEOUT_MS
  )
  group.filterClear = null
  group.filterClearRequested = false
}

async function stopBluezPhysicalDiscovery(runtime: BluezBackendRuntime, group: BluezScanGroup): Promise<void> {
  if (!group.stopDiscoveryRequested) {
    group.stopDiscoveryRequested = true
    const stopDiscovery = runtime.boundary.methods.callVoid(
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'StopDiscovery',
      []
    )
    group.stopDiscovery = stopDiscovery
    stopDiscovery.catch(() => {
      if (group.stopDiscovery === stopDiscovery) {
        group.stopDiscovery = null
        group.stopDiscoveryRequested = false
      }
    })
  }
  const stopDiscovery = group.stopDiscovery
  if (stopDiscovery === null) {
    throw contractError('lifecycle.invariant-violation', 'scan', 'bluez.scan.stop-discovery')
  }
  await awaitBluezNativePromise(stopDiscovery, runtime.now, 'bluez.scan.stop-discovery')
  group.physicalStarted = false
  group.stopDiscovery = null
  group.stopDiscoveryRequested = false
}

function trackBluezScanAcquisition(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  consumer: BluezScanConsumer,
  nativePromise: Promise<void>,
  onSuccess: () => void,
  onFailure: () => void
): void {
  nativePromise.then(
    () => {
      if (runtime.scanGroup !== group) {
        return
      }
      onSuccess()
      if (group.stopRequested && consumer.stopped === null) {
        observeScanCleanup(runtime.stopScan(consumer))
      }
    },
    () => {
      onFailure()
    }
  )
}

function bluezScanAcquisitionPending(group: BluezScanGroup): boolean {
  return (
    (group.setFilter !== null && !group.filterSettled) || (group.startDiscovery !== null && !group.discoverySettled)
  )
}

async function adoptPendingBluezScanAcquisition(runtime: BluezBackendRuntime, group: BluezScanGroup): Promise<void> {
  if (group.setFilter !== null && !group.filterSettled) {
    try {
      await awaitBluezNativePromise(group.setFilter, runtime.now, 'bluez.scan.set-filter')
      group.filterApplied = true
      group.filterSettled = true
    } catch (error) {
      if (isBluezCleanupTimeout(error)) {
        throw error
      }
      group.filterSettled = true
      group.setFilter = null
    }
  }
  if (group.startDiscovery !== null && !group.discoverySettled) {
    try {
      await awaitBluezNativePromise(group.startDiscovery, runtime.now, 'bluez.scan.start-discovery')
      group.physicalStarted = true
      group.discoverySettled = true
    } catch (error) {
      if (isBluezCleanupTimeout(error)) {
        throw error
      }
      group.discoverySettled = true
      group.startDiscovery = null
    }
  }
}

function settleBluezScanStartup(group: BluezScanGroup): void {
  if (group.startupComplete) {
    return
  }
  group.startupComplete = true
  group.settleStartup()
}

function pendingBluezScanCleanup(operation: string): CleanupRecord {
  return Object.freeze({
    state: 'release-failed',
    failures: Object.freeze([
      {
        resourceKind: 'scan',
        error: contractError('operation.timed-out', 'cleanup', operation).normalized
      }
    ])
  })
}

function isBluezCleanupTimeout(error: unknown): boolean {
  return error instanceof BackendContractError && error.normalized.code === 'operation.timed-out'
}

function observeScanCleanup(cleanup: Promise<CleanupRecord>): void {
  cleanup.catch(error => {
    console.error('[startBluezScan] Failed to stop an aborted BlueZ scan:', error)
  })
}

function assertScanAdmission(runtime: BluezBackendRuntime, options: OwnerScanOptions<string, string>): void {
  const failure = scanAdmissionFailure(runtime, options)
  if (failure !== null) {
    throw failure
  }
}

function scanAdmissionFailure(runtime: BluezBackendRuntime, options: OwnerScanOptions<string, string>): Error | null {
  if (options.signal?.aborted === true) {
    return contractError('operation.aborted', 'scan', 'bluez.scan.start')
  }
  if (options.deadline !== null && options.deadline <= runtime.now()) {
    return contractError('operation.timed-out', 'scan', 'bluez.scan.start')
  }
  return null
}

function scanStartupFailure(runtime: BluezBackendRuntime, group: BluezScanGroup): Error | null {
  if (runtime.isDestroying()) {
    return contractError('operation.cancelled-by-destroy', 'core', 'bluez.scan.start')
  }
  if (group.resetRequested) {
    return contractError('operation.reset', 'core', 'bluez.scan.start')
  }
  if (runtime.scanGroup !== group) {
    return contractError('operation.reset', 'core', 'bluez.scan.start')
  }
  return null
}
