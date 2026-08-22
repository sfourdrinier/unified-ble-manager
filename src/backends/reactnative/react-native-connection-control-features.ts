// src/backends/reactnative/react-native-connection-control-features.ts

import {
  BUILT_IN_FEATURE_IDS,
  createFeatureRegistry,
  type CapabilityLimits,
  type FeatureImplementation,
  type FeatureRegistry,
  type Limitation
} from '../../backend-contract/capabilities'
import { MAXIMUM_REQUESTED_ATT_MTU, MINIMUM_ATT_MTU } from '../../backend-contract/connection-controls'
import { contractError } from '../../backend-contract/errors'
import { version, versionRange, type SerializableRecord } from '../../backend-contract/primitives'

const connectionControlScenarioId = 'connection.rssi-and-att-mtu-capability-contract'

type ReactNativeConnectionControlPlatform = 'android' | 'apple'

/**
 * Registers the radio-owned controls without inventing a connection identifier for feature invocation.
 * Callers dispatch a real operation through `Connection.readRssi` or `Connection.requestMtu` instead.
 */
export function createReactNativeConnectionControlFeatureRegistry(
  platform: ReactNativeConnectionControlPlatform,
  implementationVersion: string
): FeatureRegistry {
  const rssiLimitation = liveQualificationLimitation('RSSI measurement')
  const rssi = createFeatureRegistration(
    BUILT_IN_FEATURE_IDS.connectionRssi,
    'limited',
    implementationVersion,
    `react-native-${platform}-rssi-dispatch-v1`,
    Object.freeze([rssiLimitation]),
    Object.freeze({ minimumRssiIntegerPrecision: Object.freeze({ maximum: 1, minimum: 1, unit: 'dBm' }) })
  )
  const requestMtu =
    platform === 'android'
      ? createFeatureRegistration(
          BUILT_IN_FEATURE_IDS.connectionRequestMtu,
          'limited',
          implementationVersion,
          'react-native-android-request-mtu-dispatch-v1',
          Object.freeze([liveQualificationLimitation('ATT MTU negotiation')]),
          Object.freeze({
            attMtu: Object.freeze({
              maximum: MAXIMUM_REQUESTED_ATT_MTU,
              minimum: MINIMUM_ATT_MTU,
              unit: 'bytes'
            })
          })
        )
      : createFeatureRegistration(
          BUILT_IN_FEATURE_IDS.connectionRequestMtu,
          'unsupported',
          implementationVersion,
          'react-native-apple-corebluetooth-mtu-capability-v1',
          Object.freeze([
            Object.freeze({
              code: 'corebluetooth-auto-negotiated-mtu',
              explanation: 'CoreBluetooth negotiates ATT MTU internally and exposes no request API to the application.',
              affectedGuarantee: 'caller-directed ATT MTU negotiation'
            })
          ]),
          Object.freeze({ attMtu: Object.freeze({ maximum: 0, minimum: null, unit: 'bytes' }) })
        )
  const phy =
    platform === 'android'
      ? createFeatureRegistration(
          BUILT_IN_FEATURE_IDS.connectionPhy,
          'limited',
          implementationVersion,
          'react-native-android-phy-dispatch-v1',
          Object.freeze([liveQualificationLimitation('LE PHY read/request')]),
          Object.freeze({ phyModes: Object.freeze({ maximum: 3, minimum: 1, unit: 'modes' }) })
        )
      : createFeatureRegistration(
          BUILT_IN_FEATURE_IDS.connectionPhy,
          'unsupported',
          implementationVersion,
          'react-native-apple-corebluetooth-phy-v1',
          Object.freeze([
            Object.freeze({
              code: 'corebluetooth-no-phy-control',
              explanation: 'CoreBluetooth exposes no caller-directed LE PHY read or request API.',
              affectedGuarantee: 'caller-directed LE PHY control'
            })
          ]),
          Object.freeze({ phyModes: Object.freeze({ maximum: 0, minimum: null, unit: 'modes' }) })
        )
  return createFeatureRegistry(Object.freeze([rssi, requestMtu, phy]))
}

function createFeatureRegistration(
  id:
    | typeof BUILT_IN_FEATURE_IDS.connectionRssi
    | typeof BUILT_IN_FEATURE_IDS.connectionRequestMtu
    | typeof BUILT_IN_FEATURE_IDS.connectionPhy,
  state: 'limited' | 'unsupported',
  implementationVersion: string,
  sourceDigest: string,
  limitations: readonly Limitation[],
  limits: CapabilityLimits
) {
  const evidenceLevel = state === 'limited' ? 'deterministic' : 'blocked'
  const selectedSchemaRange = versionRange(version('capability-schema', 1), version('capability-schema', 1))
  return Object.freeze({
    id,
    state,
    selectedSchemaRange,
    implementationOrigin: 'backend-native',
    implementation: connectionControlMetadataImplementation(id),
    tck: Object.freeze({
      suiteId: 'connection-controls',
      requiredScenarioIds: Object.freeze([connectionControlScenarioId]),
      contractRange: selectedSchemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:${evidenceLevel}`,
      evidenceLevel,
      implementationVersion,
      sourceDigest,
      scenarioIds: Object.freeze([connectionControlScenarioId]),
      limitations
    }),
    limitations,
    limits
  })
}

function connectionControlMetadataImplementation(
  featureId: string
): FeatureImplementation<SerializableRecord, SerializableRecord> {
  return Object.freeze({
    async invoke(_input: SerializableRecord): Promise<SerializableRecord> {
      throw contractError('lifecycle.invalid-state', 'capability', `${featureId}.invoke-without-connection`)
    }
  })
}

function liveQualificationLimitation(operation: string): Limitation {
  return Object.freeze({
    code: 'live-radio-qualification-pending',
    explanation: `${operation} has deterministic native-protocol coverage but no reliability-qualified live-radio receipt.`,
    affectedGuarantee: 'reliability-qualified physical-radio interoperability'
  })
}
