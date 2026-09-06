// src/backends/bluez/bluez-connection-runtime.ts

import type { OwnerScanOptions } from '../../backend-contract/advertisement'
import type { PeerAddressDescriptor } from '../../backend-contract/backend'
import {
  BackendContractError,
  contractError,
  type CleanupFailure,
  type CleanupRecord
} from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { capacity, deadline, opaqueId, type ClientId, type PeerId } from '../../backend-contract/primitives'
import { BLUEZ_ADAPTER_INTERFACE, BLUEZ_DEVICE_INTERFACE, BluezDbusMethodError } from './bluez-dbus-contract'
import { BluezConnection, BluezConnectionLease, releasedBluezCleanup } from './bluez-backend-handles'
import type { BluezBackendRuntime } from './bluez-backend-runtime'
import {
  captureCleanup,
  createPendingConnectionRecord,
  requireRecordConnection,
  scanFilterObservesAddress,
  scanFilterVariant,
  widenedDiscoveryFilterVariant
} from './bluez-runtime-models'
import {
  awaitBluezNativePromise,
  awaitSharedBluezTransition,
  isBluezCallerTerminal,
  scheduleOrphanedBluezConnectionCleanup,
  waitForBluezBoolean,
  waitForBluezInterfacePresence
} from './bluez-property-waiters'
import { startBluezScan } from './bluez-scan-runtime'
import type { BluezAddressAcquisition, BluezConnectionRecord, BluezScanGroup } from './bluez-runtime-types'

const CONNECTION_OPERATION_DRAIN_TIMEOUT_MS = 1_000
const DISCONNECT_CONFIRMATION_TIMEOUT_MS = 1_000
const ADDRESS_CONNECT_RETRY_DELAY_MS = 250

export async function destroyBluezAddressAcquisitions(runtime: BluezBackendRuntime): Promise<CleanupRecord> {
  const failures: CleanupFailure[] = []
  for (const [devicePath, acquisition] of [...runtime.addressAcquisitions.entries()]) {
    const cleanup = await captureCleanup(
      compensateOrphanedBluezAddressAcquisition(runtime, devicePath, acquisition),
      'connection',
      'bluez.destroy.address-acquisition'
    )
    failures.push(...cleanup.failures)
  }
  return failures.length === 0
    ? releasedBluezCleanup
    : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
}

export async function connectBluezConnection(
  runtime: BluezBackendRuntime,
  peerId: PeerId<string>,
  clientId: ClientId<string, string>,
  options: PublicOperationOptions
): Promise<BluezConnectionLease> {
  runtime.assertUsable('bluez.connect')
  assertConnectAdmission(runtime, options)
  const devicePath = runtime.devicePathForPeer(peerId)
  if (!devicePath.startsWith(`${String(runtime.selectedAdapter.adapterId)}/`)) {
    throw contractError('connection.not-found', 'connection', 'bluez.connect')
  }
  const addressTarget = runtime.addressTargetForPath(devicePath)
  if (addressTarget !== undefined) {
    return connectBluezAddressTarget(runtime, peerId, devicePath, addressTarget, clientId, options)
  }
  if (!runtime.store.hasInterface(devicePath, BLUEZ_DEVICE_INTERFACE)) {
    throw contractError('connection.not-found', 'connection', 'bluez.connect')
  }
  return connectBluezSharedRecord(runtime, peerId, devicePath, options)
}

/**
 * Pending connect to an out-of-band address (peer:address-targeting). The peer does not
 * need to be advertising at call time: the device object is materialized when it wakes
 * (Adapter1.ConnectDevice where the daemon supports it, otherwise an address-filtered
 * bootstrap discovery) and failed establishment attempts are retried until the caller's
 * signal aborts or deadline expires.
 */
async function connectBluezAddressTarget(
  runtime: BluezBackendRuntime,
  peerId: PeerId<string>,
  devicePath: string,
  target: PeerAddressDescriptor,
  clientId: ClientId<string, string>,
  options: PublicOperationOptions
): Promise<BluezConnectionLease> {
  for (;;) {
    runtime.assertUsable('bluez.connect.address')
    assertConnectAdmission(runtime, options)
    if (runtime.addressTargetForPath(devicePath) === undefined) {
      // A backend restart invalidated every minted peer handle while this attempt was
      // pending; the caller must mint a new address peer against the new generation.
      throw contractError('connection.not-found', 'connection', 'bluez.connect.address')
    }
    await settleRetainedBluezAddressCompensation(runtime, devicePath)
    if (!runtime.store.hasInterface(devicePath, BLUEZ_DEVICE_INTERFACE)) {
      await materializeBluezAddressDevice(runtime, devicePath, target, clientId, options)
      continue
    }
    try {
      return await connectBluezSharedRecord(runtime, peerId, devicePath, options)
    } catch (error) {
      if (!isRetriableBluezAddressConnectFailure(error)) {
        throw error
      }
      await delayBluezAddressRetry(runtime, options)
    }
  }
}

async function materializeBluezAddressDevice(
  runtime: BluezBackendRuntime,
  devicePath: string,
  target: PeerAddressDescriptor,
  clientId: ClientId<string, string>,
  options: PublicOperationOptions
): Promise<void> {
  if (!runtime.connectDeviceUnavailable) {
    const acquisition = adoptBluezAddressAcquisition(runtime, devicePath, target)
    acquisition.waiters += 1
    let adopted = false
    try {
      await awaitSharedBluezTransition(acquisition.completion, options, runtime.now, 'bluez.connect.connect-device')
      await waitForBluezInterfacePresence(runtime, devicePath, BLUEZ_DEVICE_INTERFACE, options)
      adopted = true
      return
    } catch (error) {
      if (isBluezUnknownMethodError(error)) {
        runtime.connectDeviceUnavailable = true
        if (runtime.addressAcquisitions.get(devicePath) === acquisition && acquisition.waiters <= 1) {
          runtime.addressAcquisitions.delete(devicePath)
        }
      } else if (isBluezCallerTerminal(error)) {
        throw error
      } else if (error instanceof BluezDbusMethodError) {
        await delayBluezAddressRetry(runtime, options)
        return
      } else {
        throw error
      }
    } finally {
      acquisition.waiters -= 1
      if (acquisition.waiters === 0 && !adopted) {
        scheduleOrphanedBluezAddressAcquisition(runtime, devicePath, acquisition)
      } else if (acquisition.waiters === 0 && adopted) {
        runtime.addressAcquisitions.delete(devicePath)
      }
    }
  }
  await discoverBluezAddressDevice(runtime, devicePath, target, clientId, options)
}

function adoptBluezAddressAcquisition(
  runtime: BluezBackendRuntime,
  devicePath: string,
  target: PeerAddressDescriptor
): BluezAddressAcquisition {
  const existing = runtime.addressAcquisitions.get(devicePath)
  if (existing !== undefined) {
    return existing
  }
  const connectDevice = runtime.boundary.methods.callVoid(
    String(runtime.selectedAdapter.adapterId),
    BLUEZ_ADAPTER_INTERFACE,
    'ConnectDevice',
    [
      {
        signature: 'a{sv}',
        value: Object.freeze({
          Address: { signature: 's', value: target.address },
          AddressType: { signature: 's', value: target.addressType }
        })
      }
    ]
  )
  const acquisition: BluezAddressAcquisition = {
    completion: connectDevice,
    waiters: 0,
    connectDevice,
    compensation: null
  }
  runtime.addressAcquisitions.set(devicePath, acquisition)
  return acquisition
}

function scheduleOrphanedBluezAddressAcquisition(
  runtime: BluezBackendRuntime,
  devicePath: string,
  acquisition: BluezAddressAcquisition
): void {
  acquisition.completion.then(
    () => {
      if (acquisition.waiters > 0 || runtime.addressAcquisitions.get(devicePath) !== acquisition) {
        return
      }
      compensateOrphanedBluezAddressAcquisition(runtime, devicePath, acquisition).then(
        cleanup => {
          if (cleanup.state === 'release-failed') {
            console.error('[connectBluezConnection] Late ConnectDevice compensation failed:', cleanup.failures)
          }
        },
        error => {
          console.error('[connectBluezConnection] Late ConnectDevice compensation failed:', error)
        }
      )
    },
    () => {
      if (acquisition.waiters === 0 && runtime.addressAcquisitions.get(devicePath) === acquisition) {
        runtime.addressAcquisitions.delete(devicePath)
      }
    }
  )
}

async function settleRetainedBluezAddressCompensation(runtime: BluezBackendRuntime, devicePath: string): Promise<void> {
  const acquisition = runtime.addressAcquisitions.get(devicePath)
  if (acquisition === undefined || acquisition.waiters > 0) {
    return
  }
  if (acquisition.compensation === null && !runtime.store.hasInterface(devicePath, BLUEZ_DEVICE_INTERFACE)) {
    return
  }
  const cleanup = await compensateOrphanedBluezAddressAcquisition(runtime, devicePath, acquisition)
  if (cleanup.state !== 'released') {
    throw contractError('platform.failure', 'cleanup', 'bluez.connect.address-compensation')
  }
}

async function compensateOrphanedBluezAddressAcquisition(
  runtime: BluezBackendRuntime,
  devicePath: string,
  acquisition: BluezAddressAcquisition
): Promise<CleanupRecord> {
  if (acquisition.compensation !== null) {
    return acquisition.compensation
  }
  const compensation = disconnectOrphanedBluezAddressDevice(runtime, devicePath, acquisition)
  acquisition.compensation = compensation
  try {
    const cleanup = await compensation
    if (cleanup.state === 'released') {
      if (runtime.addressAcquisitions.get(devicePath) === acquisition) {
        runtime.addressAcquisitions.delete(devicePath)
      }
    } else if (acquisition.compensation === compensation) {
      acquisition.compensation = null
    }
    return cleanup
  } catch (error) {
    if (acquisition.compensation === compensation) {
      acquisition.compensation = null
    }
    throw error
  }
}

async function disconnectOrphanedBluezAddressDevice(
  runtime: BluezBackendRuntime,
  devicePath: string,
  acquisition: BluezAddressAcquisition
): Promise<CleanupRecord> {
  try {
    await awaitBluezNativePromise(acquisition.completion, runtime.now, 'bluez.connect.address-compensation')
  } catch (error) {
    if (error instanceof BackendContractError && error.normalized.code === 'operation.timed-out') {
      return pendingBluezConnectionCleanup('bluez.connect.address-compensation')
    }
    if (acquisition.waiters === 0 && runtime.addressAcquisitions.get(devicePath) === acquisition) {
      runtime.addressAcquisitions.delete(devicePath)
    }
    return releasedBluezCleanup
  }
  if (acquisition.waiters > 0 || runtime.addressAcquisitions.get(devicePath) !== acquisition) {
    return releasedBluezCleanup
  }
  if (runtime.connectionRecords.has(devicePath) || !runtime.store.hasInterface(devicePath, BLUEZ_DEVICE_INTERFACE)) {
    runtime.addressAcquisitions.delete(devicePath)
    return releasedBluezCleanup
  }
  try {
    const disconnectMethod = runtime.boundary.methods.callVoid(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', [])
    await awaitBluezNativePromise(disconnectMethod, runtime.now, 'bluez.connect.address-compensation')
    await waitForBluezBoolean(runtime, devicePath, BLUEZ_DEVICE_INTERFACE, 'Connected', false, {
      signal: null,
      deadline: deadline(runtime.now() + DISCONNECT_CONFIRMATION_TIMEOUT_MS)
    })
  } catch (error) {
    if (
      runtime.addressAcquisitions.get(devicePath) !== acquisition ||
      !runtime.store.hasInterface(devicePath, BLUEZ_DEVICE_INTERFACE)
    ) {
      if (runtime.addressAcquisitions.get(devicePath) === acquisition) {
        runtime.addressAcquisitions.delete(devicePath)
      }
      return releasedBluezCleanup
    }
    if (error instanceof BackendContractError && error.normalized.code === 'operation.timed-out') {
      return pendingBluezConnectionCleanup('bluez.connect.address-compensation')
    }
    throw error
  }
  if (runtime.addressAcquisitions.get(devicePath) === acquisition) {
    runtime.addressAcquisitions.delete(devicePath)
  }
  return releasedBluezCleanup
}

async function discoverBluezAddressDevice(
  runtime: BluezBackendRuntime,
  devicePath: string,
  target: PeerAddressDescriptor,
  clientId: ClientId<string, string>,
  options: PublicOperationOptions
): Promise<void> {
  await waitForBluezScanGroupToSettle(runtime, options)
  const current = runtime.scanGroup
  if (current !== null && isReusableBluezScanGroup(current)) {
    const owner = current.consumers.get(String(current.ownerLeaseId))
    if (owner !== undefined && scanFilterObservesAddress(owner.options, target)) {
      await waitForBluezInterfacePresence(runtime, devicePath, BLUEZ_DEVICE_INTERFACE, options)
      return
    }
    if (owner !== undefined) {
      await widenBluezScanForAddress(runtime, current, owner, devicePath, options)
      return
    }
  }
  const lease = await startBluezScan(runtime, addressDiscoveryScanOptions(target, options), clientId)
  let presenceError: unknown = null
  try {
    await waitForBluezInterfacePresence(runtime, devicePath, BLUEZ_DEVICE_INTERFACE, options)
  } catch (error) {
    presenceError = error
  }
  const cleanup = await captureCleanup(lease.stop(), 'scan', 'bluez.connect.address-scan-stop')
  if (cleanup.state !== 'released') {
    if (presenceError !== null) {
      throw presenceError
    }
    throw contractError('scan.stop-failed', 'scan', 'bluez.connect.address-scan-stop')
  }
  if (presenceError !== null) {
    throw presenceError
  }
}

function isReusableBluezScanGroup(group: BluezScanGroup): boolean {
  return group.state === 'active' && !group.stopRequested
}

async function waitForBluezScanGroupToSettle(
  runtime: BluezBackendRuntime,
  options: PublicOperationOptions
): Promise<void> {
  const group = runtime.scanGroup
  if (group === null) {
    return
  }
  if (!group.startupComplete) {
    await awaitSharedBluezTransition(group.startupSettled, options, runtime.now, 'bluez.connect.address-scan-start')
  }
  const current = runtime.scanGroup
  if (current === null || isReusableBluezScanGroup(current)) {
    return
  }
  const owner = current.consumers.get(String(current.ownerLeaseId))
  if (owner === undefined) {
    return
  }
  await awaitSharedBluezTransition(
    runtime.stopScan(owner).then(() => undefined),
    options,
    runtime.now,
    'bluez.connect.address-scan-stop'
  )
}

async function widenBluezScanForAddress(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  owner: { readonly options: OwnerScanOptions<string, string> },
  devicePath: string,
  options: PublicOperationOptions
): Promise<void> {
  const original = scanFilterVariant(owner.options)
  group.addressWidenBorrowers += 1
  let adopted = false
  try {
    adopted = await adoptBluezAddressWiden(runtime, group, owner, options)
    await waitForBluezInterfacePresence(runtime, devicePath, BLUEZ_DEVICE_INTERFACE, options)
  } catch (error) {
    if (!adopted) {
      const widen = group.addressWiden
      if (widen !== null) {
        widen.then(
          () => {
            if (group.addressWidenBorrowers === 0) {
              beginBluezAddressWidenRestore(runtime, group, original).catch(() => undefined)
            }
          },
          () => {
            if (group.addressWiden === widen && group.addressWidenBorrowers === 0) {
              group.addressWiden = null
            }
          }
        )
      }
    }
    throw error
  } finally {
    group.addressWidenBorrowers -= 1
    if (group.addressWidenBorrowers === 0 && adopted) {
      await beginBluezAddressWidenRestore(runtime, group, original)
    }
  }
}

async function adoptBluezAddressWiden(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  owner: { readonly options: OwnerScanOptions<string, string> },
  options: PublicOperationOptions
): Promise<boolean> {
  if (group.addressWidenRestore !== null) {
    await awaitSharedBluezTransition(group.addressWidenRestore, options, runtime.now, 'bluez.connect.address-restore')
  }
  if (runtime.scanGroup !== group || group.state !== 'active' || group.stopRequested) {
    return false
  }
  let widen = group.addressWiden
  if (widen === null) {
    widen = runtime.boundary.methods.callVoid(
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'SetDiscoveryFilter',
      [widenedDiscoveryFilterVariant(owner.options)]
    )
    group.setFilter = widen
    group.filterSettled = false
    group.addressWiden = widen
  }
  await awaitSharedBluezTransition(widen, options, runtime.now, 'bluez.connect.address-widen')
  if (runtime.scanGroup !== group || group.state !== 'active' || group.stopRequested) {
    return false
  }
  if (group.setFilter === widen) {
    group.filterSettled = true
  }
  return true
}

function beginBluezAddressWidenRestore(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  original: ReturnType<typeof scanFilterVariant>
): Promise<void> {
  if (group.addressWidenBorrowers > 0) {
    return Promise.resolve()
  }
  if (group.addressWidenRestore !== null) {
    return group.addressWidenRestore
  }
  group.addressWiden = null
  let settleRestore = (): void => undefined
  const held = new Promise<void>(resolve => {
    settleRestore = resolve
  })
  group.addressWidenRestore = held
  const restore = restoreBluezScanFilter(runtime, group, original)
  restore.then(
    () => {
      settleRestore()
      if (group.addressWidenRestore === held) {
        group.addressWidenRestore = null
      }
    },
    () => {
      settleRestore()
      if (group.addressWidenRestore === held) {
        group.addressWidenRestore = null
      }
    }
  )
  return restore
}

async function restoreBluezScanFilter(
  runtime: BluezBackendRuntime,
  group: BluezScanGroup,
  original: ReturnType<typeof scanFilterVariant>
): Promise<void> {
  if (runtime.scanGroup !== group) {
    return
  }
  if (group.addressWidenBorrowers > 0) {
    return
  }
  if (group.state !== 'active' || group.stopRequested) {
    group.pendingFilterRestore = original
    return
  }
  group.pendingFilterRestore = original
  try {
    const restore = runtime.boundary.methods.callVoid(
      String(runtime.selectedAdapter.adapterId),
      BLUEZ_ADAPTER_INTERFACE,
      'SetDiscoveryFilter',
      [original]
    )
    group.setFilter = restore
    group.filterSettled = false
    await awaitBluezNativePromise(restore, runtime.now, 'bluez.connect.address-restore')
    group.filterSettled = true
    group.setFilter = null
    group.pendingFilterRestore = null
  } catch (error) {
    group.filterSettled = true
    group.setFilter = null
    console.error(
      '[connectBluezConnection] Failed to restore the BlueZ discovery filter after address widening:',
      error
    )
    throw error
  }
}

function addressDiscoveryScanOptions(
  target: PeerAddressDescriptor,
  options: PublicOperationOptions
): OwnerScanOptions<string, string> {
  return {
    // BlueZ discovery `Pattern` matches an address prefix, which keeps the bootstrap
    // discovery narrowed to the targeted peer.
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: target.address },
    duplicatePolicy: 'first',
    timestampPolicy: 'receipt-monotonic',
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(4096),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    },
    deadline: options.deadline,
    signal: options.signal,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

function isRetriableBluezAddressConnectFailure(error: unknown): boolean {
  if (error instanceof BluezDbusMethodError) {
    return true
  }
  if (!(error instanceof BackendContractError)) {
    return false
  }
  // A failed D-Bus establishment attempt (peer asleep between advertising bursts) surfaces
  // as platform.failure; the pending contract keeps trying until the caller's signal or
  // deadline ends the attempt. Stale/raced records are re-created by the next iteration.
  if (error.normalized.code === 'platform.failure') {
    return error.normalized.platform?.domain === 'bluez-dbus'
  }
  return error.normalized.code === 'connection.stale' || error.normalized.code === 'connection.not-found'
}

function isBluezUnknownMethodError(error: unknown): boolean {
  return (
    error instanceof BluezDbusMethodError &&
    (error.detail.name === 'org.freedesktop.DBus.Error.UnknownMethod' ||
      error.detail.name === 'org.bluez.Error.NotSupported')
  )
}

function delayBluezAddressRetry(runtime: BluezBackendRuntime, options: PublicOperationOptions): Promise<void> {
  return new Promise((resolve, reject) => {
    if (options.signal?.aborted === true) {
      reject(contractError('operation.aborted', 'connection', 'bluez.connect.address-retry'))
      return
    }
    const waitMs =
      options.deadline === null
        ? ADDRESS_CONNECT_RETRY_DELAY_MS
        : Math.min(ADDRESS_CONNECT_RETRY_DELAY_MS, Math.max(0, options.deadline - runtime.now()))
    let settled = false
    const abort = (): void => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      reject(contractError('operation.aborted', 'connection', 'bluez.connect.address-retry'))
    }
    const timer = setTimeout(() => {
      if (settled) return
      settled = true
      options.signal?.removeEventListener('abort', abort)
      resolve()
    }, waitMs)
    options.signal?.addEventListener('abort', abort, { once: true })
  })
}

async function connectBluezSharedRecord(
  runtime: BluezBackendRuntime,
  peerId: PeerId<string>,
  devicePath: string,
  options: PublicOperationOptions
): Promise<BluezConnectionLease> {
  let record = runtime.connectionRecords.get(devicePath)
  if (record === undefined) {
    const attachmentId = runtime.attachment().attachmentId
    const ids = runtime.identifiers()
    record = createPendingConnectionRecord(devicePath, peerId)
    const sharedRecord = record
    const connection = new BluezConnection(
      runtime,
      record,
      peerId,
      ids.connectionId(`bluez-connection-${runtime.nextConnection}`),
      opaqueId(
        String(runtime.nextConnection),
        'connection-generation',
        `${String(runtime.attachment().attachmentId)}:${devicePath}`
      )
    )
    record.connection = connection
    runtime.nextConnection += 1
    runtime.connectionRecords.set(devicePath, record)
    const alreadyConnected =
      runtime.store.optionalBooleanProperty(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connected') === true
    record.physicalLinkMayExist = alreadyConnected
    const dispatch = runtime.dispatcher.dispatch({ signal: null, deadline: null }, 'bluez.connect', async () => {
      if (!alreadyConnected) {
        sharedRecord.physicalLinkMayExist = true
        await connectBluezPhysicalLink(runtime, devicePath)
        runtime.assertAttachmentCurrent(attachmentId, 'bluez.connect.after-method')
      }
      await waitForBluezBoolean(runtime, devicePath, BLUEZ_DEVICE_INTERFACE, 'Connected', true, {
        signal: null,
        deadline: null
      })
      sharedRecord.state = 'connected'
      sharedRecord.active = true
      runtime.assertAttachmentCurrent(attachmentId, 'bluez.connect.after-confirmation')
    })
    record.transition = dispatch.completion
    record.transition.catch(error => {
      dispatch.physicalSettlement.then(() => {
        if (
          !sharedRecord.active &&
          !runtime.isDestroying() &&
          runtime.connectionRecords.get(devicePath) === sharedRecord
        ) {
          sharedRecord.physicalLinkMayExist = false
          sharedRecord.state = 'disconnected'
          runtime.connectionRecords.delete(devicePath)
        }
        console.error('[connectBluezConnection] Shared BlueZ connect transition failed:', error)
      })
    })
  }
  record.pendingConnectors += 1
  try {
    await awaitSharedBluezTransition(record.transition, options, runtime.now, 'bluez.connect.join')
  } catch (error) {
    record.pendingConnectors -= 1
    scheduleOrphanedBluezConnectionCleanup(runtime, record)
    throw error
  }
  record.pendingConnectors -= 1
  runtime.assertUsable('bluez.connect.confirmed')
  if (!record.active || record.state !== 'connected' || runtime.connectionRecords.get(devicePath) !== record) {
    throw contractError('connection.stale', 'connection', 'bluez.connect.confirmed')
  }
  const ids = runtime.identifiers()
  const leaseId = ids.leaseId(`bluez-connection-lease-${runtime.nextLease}`)
  runtime.nextLease += 1
  const lease = new BluezConnectionLease(runtime, record, leaseId, requireRecordConnection(record))
  record.leases.add(lease)
  if (record.ownerLeaseId === null) {
    record.ownerLeaseId = leaseId
  }
  return lease
}

async function connectBluezPhysicalLink(runtime: BluezBackendRuntime, devicePath: string): Promise<void> {
  try {
    await runtime.boundary.methods.callVoid(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', [])
  } catch (error) {
    if (!(error instanceof BluezDbusMethodError) || error.detail.name !== 'org.bluez.Error.AlreadyConnected') {
      throw error
    }
  }
}

function assertConnectAdmission(runtime: BluezBackendRuntime, options: PublicOperationOptions): void {
  if (options.signal?.aborted === true) {
    throw contractError('operation.aborted', 'connection', 'bluez.connect')
  }
  if (options.deadline !== null && options.deadline <= runtime.now()) {
    throw contractError('operation.timed-out', 'connection', 'bluez.connect')
  }
}

export async function disconnectBluezConnection(
  runtime: BluezBackendRuntime,
  record: BluezConnectionRecord,
  invalidate: () => void
): Promise<CleanupRecord> {
  if (!record.active && !record.physicalLinkMayExist && record.disconnectMethod === null) {
    return releasedBluezCleanup
  }
  if (record.disconnection !== null) {
    return record.disconnection
  }
  const disconnect = disconnectBluezPhysicalLink(runtime, record, invalidate)
  record.disconnection = disconnect
  try {
    const cleanup = await disconnect
    if (cleanup.state === 'release-failed' && record.disconnection === disconnect) {
      record.disconnection = null
    }
    return cleanup
  } catch (error) {
    if (record.disconnection === disconnect) {
      record.disconnection = null
    }
    throw error
  }
}

async function disconnectBluezPhysicalLink(
  runtime: BluezBackendRuntime,
  record: BluezConnectionRecord,
  invalidate: () => void
): Promise<CleanupRecord> {
  if (!(await waitForBluezConnectionOperations(runtime, record))) {
    return pendingBluezConnectionCleanup('bluez.connection.dispatcher-idle')
  }
  record.state = 'disconnecting'
  try {
    if (!record.disconnectRequested) {
      record.disconnectRequested = true
      const disconnectMethod = runtime.boundary.methods.callVoid(
        record.devicePath,
        BLUEZ_DEVICE_INTERFACE,
        'Disconnect',
        []
      )
      record.disconnectMethod = disconnectMethod
      disconnectMethod.catch(() => {
        if (record.disconnectMethod === disconnectMethod) {
          record.disconnectMethod = null
          record.disconnectRequested = false
        }
      })
    }
    const disconnectMethod = record.disconnectMethod
    if (disconnectMethod === null) {
      throw contractError('lifecycle.invariant-violation', 'connection', 'bluez.connection.disconnect-method')
    }
    await awaitBluezNativePromise(disconnectMethod, runtime.now, 'bluez.connection.disconnect-method')
    await waitForBluezBoolean(runtime, record.devicePath, BLUEZ_DEVICE_INTERFACE, 'Connected', false, {
      signal: null,
      deadline: deadline(runtime.now() + DISCONNECT_CONFIRMATION_TIMEOUT_MS)
    })
  } catch (error) {
    if (
      runtime.connectionRecords.get(record.devicePath) !== record ||
      (!record.active && !record.physicalLinkMayExist)
    ) {
      return releasedBluezCleanup
    }
    if (error instanceof BackendContractError && error.normalized.code === 'operation.timed-out') {
      return pendingBluezConnectionCleanup('bluez.connection.disconnect-confirmation')
    }
    record.state = 'disconnecting'
    throw error
  }
  record.disconnectMethod = null
  record.disconnectRequested = false
  invalidate()
  return releasedBluezCleanup
}

async function waitForBluezConnectionOperations(
  runtime: BluezBackendRuntime,
  record: BluezConnectionRecord
): Promise<boolean> {
  if (record.pendingOperations.size === 0) {
    return true
  }
  const drainDeadline = runtime.now() + CONNECTION_OPERATION_DRAIN_TIMEOUT_MS
  while (record.pendingOperations.size > 0) {
    const pending = [...record.pendingOperations.values()]
    const remaining = Math.max(0, drainDeadline - runtime.now())
    let timer: ReturnType<typeof setTimeout> | null = null
    const timeout = new Promise<boolean>(resolve => {
      timer = setTimeout(() => resolve(false), remaining)
    })
    const settled = await Promise.race([
      Promise.all(pending.map(operation => operation.physicalSettlement)).then(() => true),
      timeout
    ])
    if (timer !== null) {
      clearTimeout(timer)
    }
    if (!settled) {
      return false
    }
  }
  return true
}

function pendingBluezConnectionCleanup(operation: string): CleanupRecord {
  return Object.freeze({
    state: 'release-failed',
    failures: Object.freeze([
      {
        resourceKind: 'connection',
        error: contractError('operation.timed-out', 'cleanup', operation).normalized
      }
    ])
  })
}
