// src/react-native-app-manager.ts

import { contractError } from './backend-contract/errors'
import type { NativeBackendIdentity } from './backend-contract/identity'
import type { BleManager } from './manager/ble-manager'
import { createReactNativeBleManagerWithEnvironment } from './react-native-manager'

export interface ReactNativeBleManagerAppOptions {
  readonly clientId: string
  readonly managerId: string
  readonly hostSessionScope: string
  readonly createOwnerId?: () => string
}

/**
 * Application factory: infers RN platform, TurboModule control, and clock.
 * Tests and unusual hosts should use createReactNativeBleManagerWithEnvironment.
 */
export async function createReactNativeBleManager(
  options: ReactNativeBleManagerAppOptions
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  return createReactNativeBleManagerWithEnvironment({
    ...options,
    platform: inferReactNativeBlePlatform(),
    control: requireNativeControl(),
    now: () => performance.now()
  })
}

function inferReactNativeBlePlatform(): 'android' | 'apple' {
  const os: string = require('react-native').Platform.OS
  if (os === 'android') {
    return 'android'
  }
  if (os === 'ios') {
    return 'apple'
  }
  throw contractError('argument.invalid', 'platform', 'react-native-manager.platform')
}

function requireNativeControl(): import('./NativeUnifiedBleProtocolControl').Spec {
  const module: {
    readonly default: import('./NativeUnifiedBleProtocolControl').Spec
  } = require('./NativeUnifiedBleProtocolControl')
  return module.default
}
