// src/react-native-app-manager.ts — zero-plumbing factory over the Rust mobile owner

import { contractError } from './backend-contract/errors'
import type { BleManager } from './public/ble-manager'
import { createPublicBleManager } from './public/ble-manager'
import { rehydratePublicPromise } from './public/error-bridge'
import { createEphemeralHostIdentity, normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { bootstrapReactNativeRestorationIdentity } from './backends/reactnative/react-native-restoration'
import type { ReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core'
import type { ReactNativeRestorationAuthority } from './backends/reactnative/react-native-rust-core-restoration'
import { createNativeRandomBytesSource } from './react-native-entropy'
import { createReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core-binding'
import {
  createReactNativeManagerHost,
  type ReactNativeBlePlatform,
  type ReactNativeManagerHost
} from './react-native-manager'

/**
 * Application factory options: the public create options plus a substitute
 * native Rust core binding. When `rustCore` is absent (the ordinary case), the
 * factory resolves the production `UnifiedBleRustCore` TurboModule; every
 * radio operation, the host entropy and the restoration identity come from the
 * Rust mobile owner.
 */
export interface CreateReactNativeBleManagerOptions extends BleManagerCreateOptions {
  readonly rustCore?: ReactNativeRustCoreBinding
}

/**
 * Application factory: infers the React Native platform and clock. No
 * caller-supplied clientId/managerId/hostSessionScope — identity is supplied
 * by the trusted native host when restoration is configured.
 */
export async function createReactNativeBleManager(
  options: CreateReactNativeBleManagerOptions = {}
): Promise<BleManager> {
  return rehydratePublicPromise(
    createReactNativeApplicationHost(options).then(host =>
      createPublicBleManager(host.manager, () => performance.now())
    )
  )
}

/** The application manager plus its session services (Expo composition). */
export async function createReactNativeApplicationHost(
  options: CreateReactNativeBleManagerOptions
): Promise<ReactNativeManagerHost> {
  const { rustCore, ...publicOptions } = options
  const normalized = normalizeBleManagerCreateOptions(publicOptions)
  const platform = inferReactNativeBlePlatform()
  const binding = rustCore ?? createReactNativeRustCoreBinding({ platform })
  const randomBytes =
    normalized.randomBytes ??
    (await createNativeRandomBytesSource(async length => Array.from(await binding.randomBytes(length))))
  const ephemeral = createEphemeralHostIdentity({ randomBytes })
  let hostSessionScope = `ephemeral:${ephemeral.operationNonce}`
  let clientId = ephemeral.managerNonce
  const managerId = ephemeral.attachmentNonce
  if (normalized.instanceId !== undefined) {
    clientId = `${clientId}-${normalized.instanceId}`
  }
  let restorationAuthority: ReactNativeRestorationAuthority | undefined
  // Without a JS option, the identity the app configured natively (Info.plist
  // only) still binds the manager, as the legacy native module's did.
  const configured =
    normalized.restoration === undefined
      ? await nativeRestorationStep(() => binding.configuredRestorationIdentity())
      : null
  if (configured !== null) {
    clientId = configured.clientId
    hostSessionScope = configured.hostSessionScope
    restorationAuthority = Object.freeze({
      namespaceValue: configured.namespaceValue,
      adoptionEpoch: configured.generation,
      clientId: configured.clientId,
      hostSessionScope: configured.hostSessionScope
    })
  }
  if (normalized.restoration !== undefined) {
    const nativeIdentity = await bootstrapReactNativeRestorationIdentity(
      {
        bootstrapRestorationIdentity: request => nativeRestorationStep(() => binding.restorationIdentity(request))
      },
      normalized.restoration
    )
    clientId = nativeIdentity.clientId
    hostSessionScope = nativeIdentity.hostSessionScope
    restorationAuthority = Object.freeze({
      namespaceValue: nativeIdentity.namespaceValue,
      adoptionEpoch: nativeIdentity.generation,
      clientId: nativeIdentity.clientId,
      hostSessionScope: nativeIdentity.hostSessionScope
    })
  }
  return createReactNativeManagerHost({
    platform,
    now: () => performance.now(),
    clientId,
    managerId,
    hostSessionScope,
    adapterId: normalized.adapterId,
    diagnostics: normalized.diagnostics,
    rustCore: binding,
    ...(restorationAuthority === undefined ? {} : { restorationAuthority })
  })
}

/**
 * The legacy bootstrap reported every native restoration refusal as this one
 * identity; the owner's own answer travels as platform detail.
 */
async function nativeRestorationStep<Value>(step: () => Promise<Value>): Promise<Value> {
  try {
    return await step()
  } catch (error) {
    throw contractError('platform.failure', 'restoration', 'react-native-restoration.native-bootstrap', {
      domain: 'react-native-rust-core',
      code: 'restoration-identity',
      safeMessage: error instanceof Error ? error.message.slice(0, 1024) : String(error).slice(0, 1024),
      metadata: Object.freeze({})
    })
  }
}

function inferReactNativeBlePlatform(): ReactNativeBlePlatform {
  const os: string = require('react-native').Platform.OS
  if (os === 'android') return 'android'
  if (os === 'ios') return 'apple'
  throw contractError('argument.invalid', 'platform', 'react-native-manager.platform')
}

export type { BleManagerCreateOptions } from './public/host-identity'
