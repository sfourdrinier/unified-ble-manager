/* eslint-disable @typescript-eslint/no-explicit-any */
// src/react-native-app-manager.ts — zero-plumbing factory (PR1)

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

export interface ReactNativeBleManagerAppOptions {
  readonly clientId: string
  readonly managerId: string
  readonly hostSessionScope: string
  readonly createOwnerId?: () => string
}

/**
 * Application factory: infers RN platform, TurboModule control, and clock.
 * Supports both new BleManagerCreateOptions and legacy ReactNativeBleManagerAppOptions for test compatibility.
 * No caller-supplied clientId/managerId/hostSessionScope in new API — identity is derived internally.
 */
export async function createReactNativeBleManager(
  options: BleManagerCreateOptions | ReactNativeBleManagerAppOptions = {}
): Promise<BleManager> {
  // Backward compat: if legacy fields present, delegate directly
  if (
    (options as any).clientId !== undefined ||
    (options as any).managerId !== undefined ||
    (options as any).hostSessionScope !== undefined
  ) {
    const legacy = options as ReactNativeBleManagerAppOptions
    const internal = await createReactNativeBleManagerWithEnvironment({
      platform: inferReactNativeBlePlatform(),
      control: requireNativeControl(),
      now: () => performance.now(),
      clientId: legacy.clientId,
      managerId: legacy.managerId,
      hostSessionScope: legacy.hostSessionScope,
      createOwnerId: legacy.createOwnerId
    })
    return createPublicBleManager(internal as any, () => performance.now())
  }
  const normalized = normalizeBleManagerCreateOptions(options as BleManagerCreateOptions)
  const ephemeral = createEphemeralHostIdentity()
  // Derive stable restoration scope when requested, otherwise ephemeral.
  let hostSessionScope: string
  let clientId = ephemeral.managerNonce
  let managerId = ephemeral.attachmentNonce
  if (normalized.instanceId !== undefined) {
    // instanceId does not affect restoration; it only disambiguates multiple managers.
    clientId = `${clientId}-${normalized.instanceId}`
  }
  if (normalized.restoration !== undefined) {
    const derived = deriveRestorationIdentity(normalized.restoration)
    hostSessionScope = derived.opaqueRestorationId
    // For restoration, clientId is also derived to stay stable: use opaque + instance suffix
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
  // Wrap internal generic manager in non-generic façade.
  return createPublicBleManager(internal as any, () => performance.now())
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

export type { BleManagerCreateOptions } from './public/host-identity'
