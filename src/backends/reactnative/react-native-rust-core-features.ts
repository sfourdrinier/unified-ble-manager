// src/backends/reactnative/react-native-rust-core-features.ts
//
// Capability registrations for the React Native Rust-core backend. Every
// registration names an operation the Rust mobile owner executes through a
// frozen wire op (docs/MOBILE_RUST_WIRE.md); nothing is registered that the
// route cannot perform, and every capability the legacy React Native routes
// registered is registered here with the same state (the legacy-vs-Rust
// parity test pins the set).

import {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration,
  createFeatureRegistry,
  type BuiltInFeatureId,
  type FeatureRegistry,
  type MaximumWriteLengthFeatureImplementation
} from '../../backend-contract/capabilities'
import { contractError } from '../../backend-contract/errors'
import { version, versionRange } from '../../backend-contract/primitives'
import { createReactNativeConnectionControlFeatureRegistry } from './react-native-connection-control-features'
import { createReactNativeDescriptorFeatureRegistry } from './react-native-descriptor-features'
import {
  combineReactNativeFeatureRegistries,
  createReactNativeRestorationFeatureRegistry
} from './react-native-restoration'

export type ReactNativeRustCoreFeaturePlatform = 'android' | 'apple'

/** Facts about the running OS the host supplies (never inferred). */
export interface ReactNativeRustCoreRuntimeFacts {
  /** Android API level (`Platform.Version`); LE PHY control needs API 26+. */
  readonly androidApiLevel: number | null
}

/** The first Android API level with `BluetoothGatt.readPhy`/`setPreferredPhy`. */
export const ANDROID_PHY_API_LEVEL = 26

const catalogScenarioIds = Object.freeze(['capability.truth-limits-evidence-and-binding'])
const connectionControlScenarioIds = Object.freeze(['connection.rssi-and-att-mtu-capability-contract'])
const securityScenarioIds = Object.freeze(['security.state-pair-cancel-unpair'])
const capabilitySchemaRange = versionRange(version('capability-schema', 1), version('capability-schema', 1))

function operationRegistration(
  id: BuiltInFeatureId,
  implementationVersion: string,
  sourceDigest: string,
  tckSuiteId: string,
  requiredScenarioIds: readonly string[],
  operation: string
) {
  return createBackendOperationCapabilityRegistration({
    id,
    implementationVersion,
    sourceDigest,
    tckSuiteId,
    requiredScenarioIds: [...requiredScenarioIds],
    operation
  })
}

function phyRegistration(implementationVersion: string, available: boolean) {
  const limitations = Object.freeze([
    available
      ? Object.freeze({
          code: 'live-radio-qualification-pending',
          explanation:
            'LE PHY read/request has deterministic Rust-owner coverage but no reliability-qualified live-radio receipt.',
          affectedGuarantee: 'reliability-qualified physical-radio interoperability'
        })
      : Object.freeze({
          code: 'android-phy-api-level',
          explanation: `Android exposes LE PHY read/request from API ${ANDROID_PHY_API_LEVEL}; this device runs an older API level.`,
          affectedGuarantee: 'caller-directed LE PHY control'
        })
  ])
  const evidenceLevel = available ? ('deterministic' as const) : ('blocked' as const)
  const sourceDigest = available ? 'react-native-rust-core-android-phy-v1' : 'react-native-rust-core-android-phy-api-v1'
  return Object.freeze({
    id: BUILT_IN_FEATURE_IDS.connectionPhy,
    state: available ? ('limited' as const) : ('unsupported' as const),
    selectedSchemaRange: capabilitySchemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation: Object.freeze({
      async invoke(): Promise<never> {
        throw contractError('lifecycle.invalid-state', 'capability', 'connection:phy.invoke-without-connection')
      }
    }),
    tck: Object.freeze({
      suiteId: 'connection-controls',
      requiredScenarioIds: connectionControlScenarioIds,
      contractRange: capabilitySchemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:${evidenceLevel}`,
      evidenceLevel,
      implementationVersion,
      sourceDigest,
      scenarioIds: connectionControlScenarioIds,
      limitations
    }),
    limitations,
    limits: Object.freeze({
      phyModes: Object.freeze({ maximum: available ? 3 : 0, minimum: available ? 1 : null, unit: 'modes' })
    })
  })
}

/** The ATT maximum attribute value (Core Spec v5.x Vol 3 Part F §3.2.9). */
const ATT_MAXIMUM_ATTRIBUTE_VALUE = 512

/**
 * `gatt:maximum-write-length` (new in 5.0): the platform's own per-mode
 * answer through the Rust owner (`connection.maximum-write-length` →
 * `ReadWriteLimits`), bounded by the ATT maximum attribute value.
 * - Apple answers `CBPeripheral.maximumWriteValueLength(for:)` per type.
 * - Android answers 512 with response: the stack performs a prepared (long)
 *   write when a value exceeds one ATT payload (AOSP `gatt_cl.cc`
 *   `gatt_act_write`), and `BluetoothGatt.writeCharacteristic` refuses a
 *   value over 512 bytes from API 33 (`GATT_MAX_ATTR_LEN`). Without response
 *   it answers one ATT payload (MTU − 3) of the MTU `onMtuChanged` reported,
 *   or of the ATT default MTU 23 before any exchange: Android has no MTU
 *   readout of its own.
 */
function maximumWriteLengthRegistration(
  platform: ReactNativeRustCoreFeaturePlatform,
  implementationVersion: string,
  implementation: MaximumWriteLengthFeatureImplementation
) {
  const live = Object.freeze({
    code: 'live-radio-qualification-pending',
    explanation:
      'The maximum write length has deterministic Rust-owner coverage but no reliability-qualified live-radio receipt.',
    affectedGuarantee: 'reliability-qualified physical-radio interoperability'
  })
  const limitations = Object.freeze(
    platform === 'android'
      ? [
          Object.freeze({
            code: 'android-att-default-mtu-before-exchange',
            explanation:
              'Android exposes no ATT MTU readout: until onMtuChanged reports an exchange, a write without response is bounded by one payload of the ATT default MTU 23 (20 bytes), even when the stack negotiated a larger MTU itself.',
            affectedGuarantee: 'write-without-response length before an observed MTU exchange'
          }),
          Object.freeze({
            code: 'android-prepared-write-with-response',
            explanation:
              'A write with response longer than one ATT payload is a prepared (long) write performed by the Android stack; a peripheral that refuses Prepare Write fails it.',
            affectedGuarantee: 'write-with-response values longer than one ATT payload'
          }),
          live
        ]
      : [live]
  )
  const sourceDigest = `react-native-rust-core-${platform}-maximum-write-length-v2`
  return Object.freeze({
    id: BUILT_IN_FEATURE_IDS.maximumWriteLength,
    state: 'limited' as const,
    selectedSchemaRange: capabilitySchemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation,
    tck: Object.freeze({
      suiteId: 'tck.feature.gatt.maximum-write-length',
      requiredScenarioIds: Object.freeze(['gatt.maximum-write-length-boundaries']),
      contractRange: capabilitySchemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:deterministic`,
      evidenceLevel: 'deterministic' as const,
      implementationVersion,
      sourceDigest,
      scenarioIds: Object.freeze(['gatt.maximum-write-length-boundaries']),
      limitations
    }),
    limitations,
    limits: Object.freeze({
      maximumWriteLength: Object.freeze({ minimum: 1, maximum: ATT_MAXIMUM_ATTRIBUTE_VALUE, unit: 'bytes' })
    })
  })
}

/** The Rust-core backend's capability registry for one platform and runtime. */
export function createReactNativeRustCoreFeatureRegistry(
  platform: ReactNativeRustCoreFeaturePlatform,
  implementationVersion: string,
  facts: ReactNativeRustCoreRuntimeFacts,
  maximumWriteLength: MaximumWriteLengthFeatureImplementation
): FeatureRegistry {
  const direct = operationRegistration(
    BUILT_IN_FEATURE_IDS.connectionDirect,
    implementationVersion,
    `react-native-rust-core-${platform}-direct-connection-v1`,
    'capability.catalog-v2',
    ['scenario.scan-connect-discover-read-notify-destroy'],
    'connection:direct.invoke-without-connection'
  )
  const common = [
    createReactNativeConnectionControlFeatureRegistry(platform, implementationVersion),
    createReactNativeDescriptorFeatureRegistry(platform, implementationVersion),
    createReactNativeRestorationFeatureRegistry(platform, implementationVersion),
    createFeatureRegistry(
      Object.freeze([direct, maximumWriteLengthRegistration(platform, implementationVersion, maximumWriteLength)])
    )
  ]
  if (platform === 'apple') {
    return combineReactNativeFeatureRegistries(...common)
  }
  const phyAvailable = facts.androidApiLevel !== null && facts.androidApiLevel >= ANDROID_PHY_API_LEVEL
  const android = createFeatureRegistry(
    Object.freeze([
      operationRegistration(
        BUILT_IN_FEATURE_IDS.connectionPriority,
        implementationVersion,
        'react-native-rust-core-android-request-priority-v1',
        'connection-controls',
        connectionControlScenarioIds,
        'connection:priority.invoke-without-connection'
      ),
      phyRegistration(implementationVersion, phyAvailable),
      operationRegistration(
        BUILT_IN_FEATURE_IDS.scanPlatformOptions,
        implementationVersion,
        'react-native-rust-core-android-scan-platform-options-v1',
        'capability.catalog-v2',
        catalogScenarioIds,
        'scan:platform-options.invoke-without-scan'
      ),
      operationRegistration(
        BUILT_IN_FEATURE_IDS.peerAddressTargeting,
        implementationVersion,
        'react-native-rust-core-android-address-targeting-v1',
        'capability.catalog-v2',
        catalogScenarioIds,
        'peer:address-targeting.invoke-without-connection'
      ),
      operationRegistration(
        BUILT_IN_FEATURE_IDS.peerBonded,
        implementationVersion,
        'react-native-rust-core-android-peer-bonded-v1',
        'capability.catalog-v2',
        catalogScenarioIds,
        'peer:bonded.invoke-without-peer-directory'
      ),
      operationRegistration(
        BUILT_IN_FEATURE_IDS.peerResolveReference,
        implementationVersion,
        'react-native-rust-core-android-peer-resolve-reference-v1',
        'capability.catalog-v2',
        catalogScenarioIds,
        'peer:resolve-reference.invoke-without-peer-directory'
      ),
      operationRegistration(
        BUILT_IN_FEATURE_IDS.connectionWhenAvailable,
        implementationVersion,
        'react-native-rust-core-android-connection-when-available-v1',
        'capability.catalog-v2',
        catalogScenarioIds,
        'connection:when-available.invoke-without-connection'
      ),
      ...[
        BUILT_IN_FEATURE_IDS.securityState,
        BUILT_IN_FEATURE_IDS.securityPair,
        BUILT_IN_FEATURE_IDS.securityCancelPairing
      ].map(id =>
        operationRegistration(
          id,
          implementationVersion,
          `react-native-rust-core-android-${id.replace(':', '-')}-v1`,
          'tck.feature.security.android',
          securityScenarioIds,
          `${id}.invoke-without-security-backend`
        )
      )
    ])
  )
  return combineReactNativeFeatureRegistries(...common, android)
}
