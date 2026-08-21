// src/expo.ts — thin Expo-aware composition over the React Native factory

/**
 * Expo is a thin composition over the React Native host provider.
 * It does not reimplement BLE policy; it only validates that the
 * Expo config plugin supplied the required native configuration
 * and then delegates to the RN factory.
 *
 */

export type { BleManagerCreateOptions } from './public/host-identity'
export { normalizeBleManagerCreateOptions } from './public/host-identity'

import { normalizeBleManagerCreateOptions, type BleManagerCreateOptions } from './public/host-identity'
import type { BleManager } from './public/ble-manager'
import { createReactNativeBleManager, createReactNativeBleManagerWithEnvironment } from './react-native'
import type { ReactNativeBleManagerOptions } from './react-native-manager'

/**
 * Creates a BLE manager in an Expo-managed app. Validates the
 * single Expo restoration ID when present, then delegates to the
 * React Native factory. Not a second BLE manager implementation.
 */
export async function createExpoBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  normalizeBleManagerCreateOptions(options)
  return createReactNativeBleManager(options)
}

export async function createExpoBleManagerWithEnvironment(
  environment: ReactNativeBleManagerOptions
): Promise<BleManager> {
  const internal = await createReactNativeBleManagerWithEnvironment(environment)
  return (await import('./public/ble-manager')).createPublicBleManager(internal, environment.now)
}

export type { ReactNativeBleManagerOptions as ExpoBleManagerEnvironment }
