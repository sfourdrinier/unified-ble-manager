// src/react-native-app-manager.ts — zero-plumbing factory (PR1 final, no compatibility aliases)

import { contractError } from './backend-contract/errors'
import type { BleManager } from './public/ble-manager'
import { createPublicBleManager } from './public/ble-manager'
import { createEphemeralHostIdentity, normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { bootstrapReactNativeRestorationIdentity } from './backends/reactnative/react-native-restoration'
import { createReactNativeBleManagerWithEnvironment } from './react-native-manager'

/**
 * Application factory: infers RN platform, TurboModule control, and clock.
 * No caller-supplied clientId/managerId/hostSessionScope — identity is supplied
 * by the trusted native host when restoration is configured.
 */
export async function createReactNativeBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  const normalized = normalizeBleManagerCreateOptions(options)
  const ephemeral = createEphemeralHostIdentity()
  const control = requireNativeControl()
  let hostSessionScope = `ephemeral:${ephemeral.operationNonce}`
  let clientId = ephemeral.managerNonce
  const managerId = ephemeral.attachmentNonce
  if (normalized.instanceId !== undefined) {
    clientId = `${clientId}-${normalized.instanceId}`
  }
  if (normalized.restoration !== undefined) {
    const nativeIdentity = await bootstrapReactNativeRestorationIdentity(control, normalized.restoration)
    clientId = nativeIdentity.clientId
    hostSessionScope = nativeIdentity.hostSessionScope
  }

  const internal = await createReactNativeBleManagerWithEnvironment({
    platform: inferReactNativeBlePlatform(),
    control,
    now: () => performance.now(),
    clientId,
    managerId,
    hostSessionScope,
    adapterId: normalized.adapterId,
    diagnostics: normalized.diagnostics
  })
  return createPublicBleManager(internal, () => performance.now())
}

function inferReactNativeBlePlatform(): 'android' | 'apple' {
  const os: string = require('react-native').Platform.OS
  if (os === 'android') return 'android'
  if (os === 'ios') return 'apple'
  throw contractError('argument.invalid', 'platform', 'react-native-manager.platform')
}

function requireNativeControl(): import('./NativeUnifiedBleProtocolControl').Spec {
  const module: {
    readonly default: import('./NativeUnifiedBleProtocolControl').Spec
  } = require('./NativeUnifiedBleProtocolControl')
  return module.default
}

export type { BleManagerCreateOptions } from './public/host-identity'
