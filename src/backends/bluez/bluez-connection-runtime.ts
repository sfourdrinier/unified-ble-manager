// src/backends/bluez/bluez-connection-runtime.ts

import { BackendContractError, contractError, type CleanupRecord } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { deadline, opaqueId, type ClientId, type PeerId } from '../../backend-contract/primitives'
import { BLUEZ_DEVICE_INTERFACE, BluezDbusMethodError } from './bluez-dbus-contract'
import { BluezConnection, BluezConnectionLease, releasedBluezCleanup } from './bluez-backend-handles'
import type { BluezBackendRuntime } from './bluez-backend-runtime'
import { createPendingConnectionRecord, requireRecordConnection } from './bluez-runtime-models'
import {
  awaitBluezNativePromise,
  awaitSharedBluezTransition,
  scheduleOrphanedBluezConnectionCleanup,
  waitForBluezBoolean
} from './bluez-property-waiters'
import type { BluezConnectionRecord } from './bluez-runtime-types'

const CONNECTION_OPERATION_DRAIN_TIMEOUT_MS = 1_000
const DISCONNECT_CONFIRMATION_TIMEOUT_MS = 1_000

export async function connectBluezConnection(
  runtime: BluezBackendRuntime,
  peerId: PeerId<string>,
  _clientId: ClientId<string, string>,
  options: PublicOperationOptions
): Promise<BluezConnectionLease> {
  runtime.assertUsable('bluez.connect')
  assertConnectAdmission(runtime, options)
  const devicePath = runtime.devicePathForPeer(peerId)
  if (
    !devicePath.startsWith(`${String(runtime.selectedAdapter.adapterId)}/`) ||
    !runtime.store.hasInterface(devicePath, BLUEZ_DEVICE_INTERFACE)
  ) {
    throw contractError('connection.not-found', 'connection', 'bluez.connect')
  }
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
