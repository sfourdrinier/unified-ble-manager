// __tests__/package-surface/fixtures/host-surface.ts

import { createExpoBleManager, createExpoBleManagerWithEnvironment } from 'unified-ble-manager/expo'
import type { ExpoBleManagerEnvironment } from 'unified-ble-manager/expo'
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
import type { NativeCoreBluetoothProviderOptions } from 'unified-ble-manager/node/corebluetooth'

declare const expoEnvironment: ExpoBleManagerEnvironment
declare const tauriEnvironment: TauriBleManagerEnvironment
declare const coreBluetoothProviderOptions: NativeCoreBluetoothProviderOptions

function consume(value: unknown): void {
  void value
}

consume(createExpoBleManager())
consume(createExpoBleManagerWithEnvironment(expoEnvironment))
consume(BleProvider)
consume(useBle)
consume(useAdapterState)
consume(useBleCapability)
consume(createTauriBleManager())
consume(createTauriBleManagerWithEnvironment(tauriEnvironment))
consume(createTauriBleProvider())
consume(createCoreBluetoothBleManager({ now: () => 0 }))
consume(createNativeCoreBluetoothBackendProvider(coreBluetoothProviderOptions))
