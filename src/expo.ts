// src/expo.ts — thin Expo-aware composition over the React Native factory

/**
 * Expo is a thin composition over the React Native host provider.
 * It does not reimplement BLE policy; it only validates that the
 * Expo config plugin supplied the required native configuration
 * and then delegates to the RN factory.
 *
 * This stub satisfies the PR1 entrypoint shape. Full plugin v2,
 * permissions, hooks, and restoration wiring land in PR10.
 */

export type { BleManagerCreateOptions } from './public/host-identity'
export { normalizeBleManagerCreateOptions } from './public/host-identity'

import { normalizeBleManagerCreateOptions, type BleManagerCreateOptions } from './public/host-identity'

/**
 * Creates a BLE manager in an Expo-managed app. Validates the
 * single Expo restoration ID when present, then delegates to the
 * React Native factory. Not a second BLE manager implementation.
 */
export async function createExpoBleManager(options?: BleManagerCreateOptions): Promise<unknown> {
  const normalized = normalizeBleManagerCreateOptions(options)
  // Delegate to RN factory when available; otherwise throw a clear Expo Go message.
  // Full implementation in PR10 will import the RN factory and pass the
  // plugin-derived native configuration.
  throw new Error(
    `[unified-ble-manager/expo] createExpoBleManager is a PR10 deliverable. ` +
      `Validated options: ${JSON.stringify(normalized)}. ` +
      `Use unified-ble-manager/react-native until PR10 lands, or run with a development build (Expo Go is not supported).`
  )
}

export async function createExpoBleManagerWithEnvironment(
  _environment: unknown,
  _options?: BleManagerCreateOptions
): Promise<unknown> {
  throw new Error('[unified-ble-manager/expo] createExpoBleManagerWithEnvironment is a PR10 deliverable.')
}
