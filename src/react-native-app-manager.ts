// src/react-native-app-manager.ts — zero-plumbing factory (PR1 final, no compatibility aliases)

import { contractError } from './backend-contract/errors'
import type { BleManager } from './public/ble-manager'
import { createPublicBleManager } from './public/ble-manager'
import {
  createEphemeralHostIdentity,
  deriveRestorationIdentity,
  normalizeBleManagerCreateOptions
} from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { createReactNativeBleManagerWithEnvironment } from './react-native-manager'

/**
 * Application factory: infers RN platform, TurboModule control, and clock.
 * No caller-supplied clientId/managerId/hostSessionScope — identity is derived internally.
 */
export async function createReactNativeBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  const normalized = normalizeBleManagerCreateOptions(options)
  const ephemeral = createEphemeralHostIdentity()
  let hostSessionScope: string
  let clientId = ephemeral.managerNonce
  let managerId = ephemeral.attachmentNonce
  if (normalized.instanceId !== undefined) {
    clientId = `${clientId}-${normalized.instanceId}`
  }
  if (normalized.restoration !== undefined) {
    const derived = deriveRestorationIdentity(normalized.restoration)
    hostSessionScope = derived.opaqueRestorationId
    clientId = derived.opaqueRestorationId.slice(0, 16) + (normalized.instanceId ? `-${normalized.instanceId}` : '')
    managerId = derived.opaqueRestorationId.slice(0, 16) + '-mgr'
  } else {
    hostSessionScope = `ephemeral:${ephemeral.operationNonce}`
  }

  const internal = await createReactNativeBleManagerWithEnvironment({
    platform: inferReactNativeBlePlatform(),
    control: requireNativeControl(),
    now: () => performance.now(),
    clientId,
    managerId,
    hostSessionScope
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
