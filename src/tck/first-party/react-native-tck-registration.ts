// src/tck/first-party/react-native-tck-registration.ts
//
// The React Native first-party TCK legs run the Rust route: the production
// `createReactNativeRustCoreBinding` (and so the production serializer) over a
// deterministic `UnifiedBleRustCore` module the caller supplies, under the
// production Rust-core provider. No legacy provider or protocol control is
// involved.

import type { Spec as NativeUnifiedBleRustCore } from '../../NativeUnifiedBleRustCore'
import { createReactNativeRustCoreBinding } from '../../backends/reactnative/react-native-rust-core-binding'
import { createReactNativeRustCoreBackendProvider } from '../../backends/reactnative/react-native-rust-core-provider'
import type { ReactNativeRestorationAuthority } from '../../backends/reactnative/react-native-rust-core-restoration'
import {
  reactNativeAndroidDefaultAdapterId,
  reactNativeAppleDefaultAdapterId,
  REACT_NATIVE_ANDROID_BACKEND_ID,
  REACT_NATIVE_APPLE_BACKEND_ID
} from '../../backends/reactnative/react-native-platform-identity'
import { BUILT_IN_FEATURE_IDS } from '../../backend-contract/capabilities'
import type { NativeBackendIdentity } from '../../backend-contract/identity'
import { opaqueId, type ClientId, type SerializableRecord } from '../../backend-contract/primitives'
import type {
  TckControllerAction,
  TckFeatureScenarioAdapters,
  TckScenarioController,
  TckScenarioId
} from '../contracts'
import type { FirstPartyBackendTckRegistration } from './first-party-tck-registry'

/** The characteristic a deterministic notification is emitted on. */
export interface DeterministicReactNativeCharacteristicAddress {
  readonly nativePeerId: string
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence: number
}

/** Controller hooks into the deterministic native module (radio events the TCK drives). */
export interface DeterministicReactNativeTckBoundary {
  /** Seeds the native restoration journal (issue #212); absent when the leg cannot restore. */
  seedRestorationJournal?(): void
  emitAdvertisement(): void
  emitNotification(address: DeterministicReactNativeCharacteristicAddress, bytes: Uint8Array): void
  prepareSecurityCancellation?(): void
}

export interface DeterministicReactNativeAppleTckBoundary extends DeterministicReactNativeTckBoundary {
  seedRestorationJournal(): void
}

interface ReactNativeFirstPartyTckOptions {
  /** A deterministic `UnifiedBleRustCore` module speaking `ubm-mobile-wire/1`. */
  readonly native: NativeUnifiedBleRustCore
  readonly now: () => number
  readonly nativePeerId: string
  readonly boundary: DeterministicReactNativeTckBoundary
  readonly createOwnerId?: () => string
}

export interface ReactNativeAndroidSecurityTckOptions {
  readonly customCeremonySupported: boolean
  readonly supportsAlreadyUnpaired: boolean
  readonly supportsCancellation: boolean
  readonly supportsUnpair: boolean
}

export interface ReactNativeAndroidFirstPartyTckRegistrationOptions extends ReactNativeFirstPartyTckOptions {
  /**
   * What the deterministic module proves for the Android security suite. The
   * Rust route always registers Android security, so the suite always runs;
   * absent, the system-ceremony defaults apply (cancellation, no custom
   * ceremony, no unpair).
   */
  readonly security?: ReactNativeAndroidSecurityTckOptions
  /** Android API level the deterministic host reports (PHY needs 26+). */
  readonly androidApiLevel?: number
}

export interface ReactNativeAppleFirstPartyTckRegistrationOptions
  extends Omit<ReactNativeFirstPartyTckOptions, 'boundary'> {
  readonly boundary: DeterministicReactNativeAppleTckBoundary
}

const reactNativeProviderScenarioIds: readonly TckScenarioId[] = Object.freeze([
  'identity.provider-loadability-and-adapter-availability',
  'identity.adapter-selection-and-unique-instance',
  'identity.valid-all-axis-negotiation',
  'identity.version-skew-and-malformed-offers',
  'capability.truth-limits-evidence-and-binding',
  'gatt.duplicate-uuid-occurrences-route-exactly',
  'scenario.scan-connect-discover-read-notify-destroy'
])

const connectionControlsFeatureSuite = Object.freeze({
  suiteId: 'connection-controls',
  scenarioIds: Object.freeze(['connection.rssi-and-att-mtu-capability-contract'] as const)
})

const maximumWriteLengthFeatureSuite = Object.freeze({
  suiteId: 'tck.feature.gatt.maximum-write-length',
  scenarioIds: Object.freeze<TckScenarioId[]>(['gatt.maximum-write-length-boundaries'])
})

const descriptorOperationsFeatureSuite = Object.freeze({
  suiteId: 'descriptor-operations',
  scenarioIds: Object.freeze<TckScenarioId[]>(['gatt.descriptor-discovery-read-write'])
})

const restorationFeatureSuite = Object.freeze({
  suiteId: 'restoration',
  scenarioIds: Object.freeze([
    'restoration.provider-journal-adoption-and-rejection',
    'restoration.presence-observation-arms-known-peer'
  ] as const)
})

const androidSecurityFeatureSuite = Object.freeze({
  suiteId: 'tck.feature.security.android',
  scenarioIds: Object.freeze(['security.state-pair-cancel-unpair' as const])
})

const defaultAndroidSecurityTck: ReactNativeAndroidSecurityTckOptions = Object.freeze({
  customCeremonySupported: false,
  supportsAlreadyUnpaired: false,
  supportsCancellation: true,
  supportsUnpair: false
})

/** Registers Android's Rust route with its deterministic native module. */
export function createReactNativeAndroidFirstPartyTckRegistration(
  options: ReactNativeAndroidFirstPartyTckRegistrationOptions
): FirstPartyBackendTckRegistration {
  let authority: ReactNativeRestorationAuthority | null = null
  const provider = createReactNativeRustCoreBackendProvider({
    platform: 'android',
    binding: createReactNativeRustCoreBinding({ platform: 'android', native: options.native }),
    owner: 'react-native-android-tck',
    now: options.now,
    runtime: { androidApiLevel: options.androidApiLevel ?? 34 },
    restorationAuthority: () => authority,
    ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
  })
  return {
    backendId: REACT_NATIVE_ANDROID_BACKEND_ID,
    factory: {
      backendId: REACT_NATIVE_ANDROID_BACKEND_ID,
      provider,
      selection: Object.freeze({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() }),
      providerOnlyIdentityScenarios: true,
      staleSelection: Object.freeze({
        selectedAdapterId: opaqueId('stale-react-native-android-adapter', 'adapter', 'react-native-android')
      }),
      create: async _context => {
        const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
        const publicPeerId = backend.peerIdForNativeId(options.nativePeerId)
        return {
          backend,
          controller: createReactNativeController(options.boundary, options.nativePeerId, options.now),
          featureScenarioAdapters: Object.freeze({
            connectionControls: Object.freeze({ requestedMtu: 247 }),
            restoration: Object.freeze({
              createCapability: (clientId: ClientId<string, string>) => {
                authority = Object.freeze({
                  namespaceValue: ANDROID_TCK_NAMESPACE,
                  adoptionEpoch: ANDROID_TCK_EPOCH,
                  clientId: String(clientId),
                  hostSessionScope: ANDROID_TCK_HOST_SESSION
                })
                return Object.freeze({
                  client: Object.freeze({ clientId, hostSessionScope: ANDROID_TCK_HOST_SESSION }),
                  coordinator: provider.restoration
                })
              },
              createRequest: (identity: NativeBackendIdentity<string>) =>
                Object.freeze({
                  namespace: ANDROID_TCK_NAMESPACE,
                  attachmentId: identity.attachment.attachmentId,
                  expectedBackendInstanceId: identity.attachment.backendInstanceId,
                  expectedEpoch: opaqueId(ANDROID_TCK_EPOCH, 'restoration-epoch', 'tck'),
                  expectedVersions: identity.versions
                }),
              seedJournal: (controller: TckScenarioController) =>
                controller.perform('seed-restoration-journal', Object.freeze({}))
            }),
            presence: Object.freeze({
              observeKnownPeer: () => backend.hostServices.observePresence({ peerId: options.nativePeerId }),
              unobserveKnownPeer: () => backend.hostServices.unobservePresence({ peerId: options.nativePeerId })
            }),
            security: Object.freeze({
              peerId: publicPeerId,
              ...(options.security ?? defaultAndroidSecurityTck),
              prepareCancellation: () => options.boundary.prepareSecurityCancellation?.()
            })
          }),
          dispose: () => backend.destroy()
        }
      }
    },
    suites: Object.freeze([
      Object.freeze({
        suiteId: 'react-native-android-provider-contract-v1',
        baseScenarioIds: reactNativeProviderScenarioIds
      })
    ]),
    featureSuites: Object.freeze([
      connectionControlsFeatureSuite,
      maximumWriteLengthFeatureSuite,
      descriptorOperationsFeatureSuite,
      restorationFeatureSuite,
      androidSecurityFeatureSuite
    ]),
    capabilityExclusions: Object.freeze([])
  }
}

const ANDROID_TCK_NAMESPACE = 'unified-ble.react-native.android.tck'
const ANDROID_TCK_EPOCH = 'react-native-android-tck-restoration-epoch'
const ANDROID_TCK_HOST_SESSION = 'react-native-android-tck-session'

const APPLE_TCK_NAMESPACE = 'unified-ble.react-native.apple.tck'
const APPLE_TCK_EPOCH = 'react-native-apple-tck-restoration-epoch'
const APPLE_TCK_HOST_SESSION = 'react-native-apple-tck-session'

/**
 * Registers Apple's Rust route with its deterministic native module, including
 * RSSI and CoreBluetooth state-restoration adoption under a deterministic
 * app-declared authority (the client the TCK authenticates).
 */
export function createReactNativeAppleFirstPartyTckRegistration(
  options: ReactNativeAppleFirstPartyTckRegistrationOptions
): FirstPartyBackendTckRegistration {
  let authority: ReactNativeRestorationAuthority | null = null
  const provider = createReactNativeRustCoreBackendProvider({
    platform: 'apple',
    binding: createReactNativeRustCoreBinding({ platform: 'apple', native: options.native }),
    owner: 'react-native-apple-tck',
    now: options.now,
    runtime: { androidApiLevel: null },
    restorationAuthority: () => authority,
    ...(options.createOwnerId === undefined ? {} : { createOwnerId: options.createOwnerId })
  })
  return {
    backendId: REACT_NATIVE_APPLE_BACKEND_ID,
    factory: {
      backendId: REACT_NATIVE_APPLE_BACKEND_ID,
      provider,
      selection: Object.freeze({ selectedAdapterId: reactNativeAppleDefaultAdapterId() }),
      providerOnlyIdentityScenarios: true,
      staleSelection: Object.freeze({
        selectedAdapterId: opaqueId('stale-react-native-apple-adapter', 'adapter', 'react-native-apple')
      }),
      create: async _context => {
        const backend = await provider.create({ selectedAdapterId: reactNativeAppleDefaultAdapterId() })
        const featureScenarioAdapters = Object.freeze<
          TckFeatureScenarioAdapters<string, NativeBackendIdentity<string>>
        >({
          connectionControls: Object.freeze({ requestedMtu: 247 }),
          restoration: Object.freeze({
            createCapability: (clientId: ClientId<string, string>) => {
              authority = Object.freeze({
                namespaceValue: APPLE_TCK_NAMESPACE,
                adoptionEpoch: APPLE_TCK_EPOCH,
                clientId: String(clientId),
                hostSessionScope: APPLE_TCK_HOST_SESSION
              })
              return Object.freeze({
                client: Object.freeze({ clientId, hostSessionScope: APPLE_TCK_HOST_SESSION }),
                coordinator: provider.restoration
              })
            },
            createRequest: (identity: NativeBackendIdentity<string>) =>
              Object.freeze({
                namespace: APPLE_TCK_NAMESPACE,
                attachmentId: identity.attachment.attachmentId,
                expectedBackendInstanceId: identity.attachment.backendInstanceId,
                expectedEpoch: opaqueId(APPLE_TCK_EPOCH, 'restoration-epoch', 'tck'),
                expectedVersions: identity.versions
              }),
            seedJournal: (controller: TckScenarioController) =>
              controller.perform('seed-restoration-journal', Object.freeze({}))
          })
        })
        return {
          backend,
          controller: createReactNativeController(options.boundary, options.nativePeerId, options.now),
          featureScenarioAdapters,
          dispose: () => backend.destroy()
        }
      }
    },
    suites: Object.freeze([
      Object.freeze({
        suiteId: 'react-native-apple-provider-contract-v1',
        baseScenarioIds: reactNativeProviderScenarioIds
      })
    ]),
    featureSuites: Object.freeze([
      connectionControlsFeatureSuite,
      maximumWriteLengthFeatureSuite,
      descriptorOperationsFeatureSuite,
      restorationFeatureSuite
    ]),
    capabilityExclusions: Object.freeze([
      Object.freeze({
        featureId: BUILT_IN_FEATURE_IDS.connectionRequestMtu,
        state: 'unsupported',
        reason: 'CoreBluetooth negotiates ATT MTU internally and exposes no caller-directed request operation.'
      })
    ])
  }
}

function createReactNativeController(
  boundary: DeterministicReactNativeTckBoundary,
  nativePeerId: string,
  now: () => number
): TckScenarioController {
  const availableActions: readonly TckControllerAction[] = Object.freeze([
    'queue-advertisement',
    'emit-notification',
    'seed-restoration-journal'
  ])
  return Object.freeze({
    availableActions,
    now,
    settle: <Value>(promise: Promise<Value>) => promise,
    flush: flushMicrotasks,
    perform: async (action: TckControllerAction, input: SerializableRecord) => {
      if (action === 'queue-advertisement') {
        requireEmptyInput(action, input)
        boundary.emitAdvertisement()
        return
      }
      if (action === 'emit-notification') {
        boundary.emitNotification(
          {
            nativePeerId,
            serviceUuid: stringField(action, input, 'serviceUuid'),
            serviceOccurrence: nonNegativeIntegerField(action, input, 'serviceOccurrence'),
            characteristicUuid: stringField(action, input, 'characteristicUuid'),
            characteristicOccurrence: nonNegativeIntegerField(action, input, 'characteristicOccurrence')
          },
          bytesField(action, input, 'value')
        )
        return
      }
      if (action === 'seed-restoration-journal' && isRestorationCapableBoundary(boundary)) {
        requireEmptyInput(action, input)
        boundary.seedRestorationJournal()
        return
      }
      throw new Error(`React Native deterministic boundary cannot perform ${action}`)
    }
  })
}

function isRestorationCapableBoundary(
  boundary: DeterministicReactNativeTckBoundary
): boundary is DeterministicReactNativeAppleTckBoundary {
  return 'seedRestorationJournal' in boundary && typeof boundary.seedRestorationJournal === 'function'
}

async function flushMicrotasks(): Promise<void> {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

function requireEmptyInput(action: string, input: SerializableRecord): void {
  if (Object.keys(input).length !== 0) {
    throw new Error(`${action} must not receive input`)
  }
}

function stringField(action: string, input: SerializableRecord, field: string): string {
  const value = input[field]
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`${action}.${field} must be a non-empty string`)
  }
  return value
}

function nonNegativeIntegerField(action: string, input: SerializableRecord, field: string): number {
  const value = input[field]
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${action}.${field} must be a non-negative safe integer`)
  }
  return value
}

function bytesField(action: string, input: SerializableRecord, field: string): Uint8Array {
  const value = input[field]
  if (!(value instanceof Uint8Array)) {
    throw new Error(`${action}.${field} must be Uint8Array`)
  }
  return new Uint8Array(value)
}
