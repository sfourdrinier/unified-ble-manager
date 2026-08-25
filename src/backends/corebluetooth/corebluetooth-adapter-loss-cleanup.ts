// src/backends/corebluetooth/corebluetooth-adapter-loss-cleanup.ts

import type { CleanupFailure, CleanupRecord } from '../../backend-contract/errors'
import { cleanupFailureDetail, releasedCleanup } from './corebluetooth-handles'
import type { CoreBluetoothGattOperations } from './corebluetooth-gatt-operations'
import type { ConnectionRecord, PhysicalSubscription, ScanConsumer, ScanGroup } from './corebluetooth-backend'

export interface CoreBluetoothAdapterLossCleanupState {
  readonly scanGroup: ScanGroup | null
  readonly subscriptions: Map<string, PhysicalSubscription>
  readonly connections: Map<string, ConnectionRecord>
  readonly gattOperations: CoreBluetoothGattOperations
  stopNativeScan(group: ScanGroup, operation: string): Promise<CleanupRecord>
  disconnectNative(
    record: ConnectionRecord,
    operation: string,
    preservePhysicalSubscriptions: boolean
  ): Promise<CleanupRecord>
  releaseScanConsumerAdmission(consumer: ScanConsumer): void
  clearScanGroup(group: ScanGroup): void
}

/** Releases every native resource while retaining failed ownership for the next adapter-loss retry. */
export async function releaseCoreBluetoothAdapterLossResources(
  state: CoreBluetoothAdapterLossCleanupState
): Promise<CleanupRecord> {
  const failures: CleanupFailure[] = []
  const group = state.scanGroup
  if (group !== null) {
    group.state = 'stopping'
    for (const consumer of group.consumers.values()) {
      state.releaseScanConsumerAdmission(consumer)
      consumer.stream.closeWithReason('source-failed')
    }
    try {
      const cleanup = await state.stopNativeScan(group, 'direct-gatt.adapter-loss.stop-scan')
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
      } else {
        group.consumers.clear()
        state.clearScanGroup(group)
      }
    } catch (error) {
      failures.push(cleanupFailureDetail('scan', 'direct-gatt.adapter-loss.stop-scan', error))
    }
  }
  for (const physical of [...state.subscriptions.values()]) {
    for (const subscription of physical.consumers) {
      subscription.stream.closeWithReason('source-failed')
      subscription.removed = true
    }
    physical.consumers.clear()
    const cleanup = await state.gattOperations.stopPhysicalSubscription(physical)
    failures.push(...cleanup.failures)
  }
  for (const record of [...state.connections.values()]) {
    record.state = 'disconnecting'
    try {
      const cleanup = await state.disconnectNative(record, 'direct-gatt.adapter-loss.disconnect', true)
      failures.push(...cleanup.failures)
      if (cleanup.state === 'release-failed') {
        record.state = 'connected'
      }
    } catch (error) {
      record.state = 'connected'
      failures.push(cleanupFailureDetail('connection', 'direct-gatt.adapter-loss.disconnect', error))
    }
  }
  return failures.length === 0
    ? releasedCleanup
    : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
}
