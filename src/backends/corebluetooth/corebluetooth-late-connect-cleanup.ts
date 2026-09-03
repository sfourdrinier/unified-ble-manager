// src/backends/corebluetooth/corebluetooth-late-connect-cleanup.ts

import { BackendContractError } from '../../backend-contract/errors'
import type { CoreBluetoothBoundary } from './corebluetooth-boundary'
import { withCoreBluetoothCleanupTimeout } from './corebluetooth-cleanup'
import type { ConnectionRecord } from './corebluetooth-backend'

/** Keeps a cancelled connection reservation until its physical link is proven released. */
export async function releaseLateCoreBluetoothConnection(
  boundary: CoreBluetoothBoundary,
  connections: Map<string, ConnectionRecord>,
  record: ConnectionRecord
): Promise<boolean> {
  record.state = 'cleanup-failed'
  if (boundary.connectionState(record.nativePeerId) === 'disconnected') {
    if (connections.get(record.nativePeerId) === record) connections.delete(record.nativePeerId)
    return true
  }
  let nativeDisconnect = record.nativeDisconnect
  if (nativeDisconnect === null) {
    nativeDisconnect = boundary.disconnect(record.nativePeerId)
    record.nativeDisconnect = nativeDisconnect
    nativeDisconnect.then(
      () => {
        if (record.nativeDisconnect === nativeDisconnect) record.nativeDisconnect = null
      },
      () => {
        if (record.nativeDisconnect === nativeDisconnect) record.nativeDisconnect = null
      }
    )
  }
  try {
    await withCoreBluetoothCleanupTimeout(() => nativeDisconnect, 'direct-gatt.connect.late-cleanup')
  } catch (error) {
    if (error instanceof BackendContractError && error.normalized.code === 'operation.timed-out') {
      return false
    }
    throw error
  }
  if (boundary.connectionState(record.nativePeerId) === 'connected') return false
  if (connections.get(record.nativePeerId) === record) connections.delete(record.nativePeerId)
  return true
}
