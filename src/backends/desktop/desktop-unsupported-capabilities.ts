// src/backends/desktop/desktop-unsupported-capabilities.ts
//
// Capabilities a desktop OS cannot provide, registered `unsupported` WITH
// the reason the legacy backend gave (LEGACY-AUDIT-1 #66). An unregistered
// capability answers a bare `capability.unsupported`; a registered one tells
// the caller why, which is the legacy contract.

import {
  BUILT_IN_FEATURE_IDS,
  type CapabilityLimits,
  type FeatureId,
  type FeatureRegistry,
  type Limitation
} from '../../backend-contract/capabilities'
import { contractError } from '../../backend-contract/errors'
import { version, versionRange } from '../../backend-contract/primitives'

const capabilitySchemaRange = versionRange(version('capability-schema', 1), version('capability-schema', 1))
const connectionControlScenarioIds = Object.freeze(['connection.rssi-and-att-mtu-capability-contract'])

/** An `unsupported` connection-control row that carries its limitation. */
export function unsupportedConnectionControlRegistration(
  id: FeatureId,
  implementationVersion: string,
  sourceDigest: string,
  limitations: readonly Limitation[],
  limits: CapabilityLimits
): FeatureRegistry['registrations'][number] {
  return Object.freeze({
    id,
    state: 'unsupported' as const,
    selectedSchemaRange: capabilitySchemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation: Object.freeze({
      async invoke(): Promise<never> {
        throw contractError('capability.unsupported', 'capability', `${id}.unsupported`)
      }
    }),
    tck: Object.freeze({
      suiteId: 'connection-controls',
      requiredScenarioIds: connectionControlScenarioIds,
      contractRange: capabilitySchemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:blocked`,
      evidenceLevel: 'blocked' as const,
      implementationVersion,
      sourceDigest,
      scenarioIds: connectionControlScenarioIds,
      limitations
    }),
    limitations,
    limits
  })
}

function limitation(code: string, explanation: string, affectedGuarantee: string): readonly Limitation[] {
  return Object.freeze([Object.freeze({ code, explanation, affectedGuarantee })])
}

/**
 * CoreBluetooth's ATT MTU and PHY rows, with the legacy CoreBluetooth
 * backend's codes and explanations (corebluetooth-runtime-capabilities.ts).
 */
export function createCoreBluetoothUnsupportedRegistrations(
  implementationVersion: string
): readonly FeatureRegistry['registrations'][number][] {
  return Object.freeze([
    unsupportedConnectionControlRegistration(
      BUILT_IN_FEATURE_IDS.connectionRequestMtu,
      implementationVersion,
      'corebluetooth-auto-negotiated-mtu-v1',
      limitation(
        'corebluetooth-auto-negotiated-mtu',
        'CoreBluetooth negotiates ATT MTU internally and exposes no request API to the application.',
        'caller-directed ATT MTU negotiation'
      ),
      Object.freeze({ attMtu: Object.freeze({ minimum: null, maximum: 0, unit: 'bytes' }) })
    ),
    unsupportedConnectionControlRegistration(
      BUILT_IN_FEATURE_IDS.connectionEffectiveMtu,
      implementationVersion,
      'corebluetooth-effective-mtu-unavailable-v1',
      limitation(
        'effective-mtu-boundary-unavailable',
        'This CoreBluetooth boundary exposes no authoritative current ATT MTU observation.',
        'current effective ATT MTU observation'
      ),
      Object.freeze({ attMtu: Object.freeze({ minimum: null, maximum: 0, unit: 'bytes' }) })
    ),
    unsupportedConnectionControlRegistration(
      BUILT_IN_FEATURE_IDS.connectionPhy,
      implementationVersion,
      'corebluetooth-phy-unavailable-v2',
      limitation(
        'corebluetooth-phy-runtime-unavailable',
        'The instantiated CoreBluetooth boundary did not report an executable LE PHY read/request capability.',
        'caller-directed LE PHY control'
      ),
      Object.freeze({ phyModes: Object.freeze({ maximum: 0, minimum: null, unit: 'modes' }) })
    )
  ])
}
