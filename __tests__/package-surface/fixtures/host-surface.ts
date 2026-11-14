// __tests__/package-surface/fixtures/host-surface.ts

import { createExpoBleManager, createExpoBleManagerWithEnvironment } from 'unified-ble-manager/expo'
import type { ExpoBleManager, ExpoBleManagerEnvironment } from 'unified-ble-manager/expo'
import { BleProvider, useAdapterState, useBle, useBleCapability } from 'unified-ble-manager/react'
import {
  createTauriBleManager,
  createTauriBleManagerWithEnvironment,
  createTauriBleProvider
} from 'unified-ble-manager/tauri'
import type { TauriBleManagerEnvironment } from 'unified-ble-manager/tauri'
import {
  createCoreBluetoothBleManager,
  createNativeCoreBluetoothBackendProvider
} from 'unified-ble-manager/node/corebluetooth'
import { openDesktopScanFailureRecovery } from 'unified-ble-manager/node/corebluetooth'
import type { NativeCoreBluetoothProviderOptions } from 'unified-ble-manager/node/corebluetooth'
import type {
  NodeTerminalEvidenceRecovery,
  NodeTerminalEvidenceRecoveryRecord,
  NodeTerminalEvidenceRecoveryValue
} from 'unified-ble-manager/node/corebluetooth'
import type {
  DesktopProcessHost,
  DesktopContinuationRecordingController,
  DesktopScanFailureRecovery,
  DesktopScanFailureRecoveryChunk,
  DesktopPeerNotificationFailureRecovery
} from 'unified-ble-manager/node/corebluetooth'

declare const expoEnvironment: ExpoBleManagerEnvironment
declare const tauriEnvironment: TauriBleManagerEnvironment
declare const coreBluetoothProviderOptions: NativeCoreBluetoothProviderOptions

// Existing 5.2.1 structural host implementations did not expose the new
// opt-in ordered/recovery namespaces. They remain valid patch-line values.
declare const legacyExpoBleManager: Omit<ExpoBleManager, 'orderedRecording' | 'orderedRecovery'>
const compatibleExpoBleManager: ExpoBleManager = legacyExpoBleManager
declare const legacyDesktopProcessHost: Omit<DesktopProcessHost, 'peerFailureRecovery'>
const compatibleDesktopProcessHost: DesktopProcessHost = legacyDesktopProcessHost
declare const legacyDesktopRecordingController: Omit<DesktopContinuationRecordingController, 'releaseIdle'>
const compatibleDesktopRecordingController: DesktopContinuationRecordingController = legacyDesktopRecordingController

function consume(value: unknown): void {
  void value
}

consume(createExpoBleManager())
consume(createExpoBleManagerWithEnvironment(expoEnvironment))
consume(compatibleExpoBleManager)
consume(compatibleDesktopProcessHost)
consume(compatibleDesktopRecordingController)
consume(BleProvider)
consume(useBle)
consume(useAdapterState)
consume(useBleCapability)
consume(createTauriBleManager())
consume(createTauriBleManagerWithEnvironment(tauriEnvironment))
consume(createTauriBleProvider())
consume(createCoreBluetoothBleManager({ now: () => 0 }))
consume(createNativeCoreBluetoothBackendProvider(coreBluetoothProviderOptions))
consume(openDesktopScanFailureRecovery)

declare const nodeTerminalEvidenceRecovery: NodeTerminalEvidenceRecovery
declare const nodeTerminalEvidenceRecord: NodeTerminalEvidenceRecoveryRecord
declare const nodeTerminalEvidenceValue: NodeTerminalEvidenceRecoveryValue
declare const desktopScanFailureRecovery: DesktopScanFailureRecovery
declare const desktopScanFailureChunk: DesktopScanFailureRecoveryChunk
declare const desktopPeerFailureRecovery: DesktopPeerNotificationFailureRecovery

consume(nodeTerminalEvidenceRecovery)
consume(nodeTerminalEvidenceRecord)
consume(nodeTerminalEvidenceValue)
consume(desktopScanFailureRecovery)
consume(desktopScanFailureChunk)
consume(desktopPeerFailureRecovery)

async function consumeNodeTerminalEvidenceRecovery(): Promise<void> {
  const manager = await createCoreBluetoothBleManager({ now: () => 0 })
  const status = await manager.terminalEvidenceRecovery.listWithStatus()
  consume(status.busyProducers)
  const records = await manager.terminalEvidenceRecovery.list()
  for (const record of records) {
    const value = await manager.terminalEvidenceRecovery.read(record.handle, record.byteLength)
    await manager.terminalEvidenceRecovery.acknowledge(record.handle)
    consume(value)
  }
}

consume(consumeNodeTerminalEvidenceRecovery)
